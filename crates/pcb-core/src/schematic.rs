//! Schematic model.
//!
//! The schematic is a *netlist with layout hints*. The agent declares
//! symbols (by kind) and conectividad por nets — `R1.1` and `U1.8`
//! are both on net `VCC`, etc. Wires are not stored: rendering uses a
//! labels-only style (each pin stub carries its net name), which is a
//! valid `KiCad` convention and keeps the model identical to what the
//! agent reasons about.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::board::{CopperLayer, Id};
use crate::geometry::Point;
use crate::units::Length;

/// Side of a symbol body where a pin stub points outwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PinSide {
    Left,
    Right,
    Top,
    Bottom,
}

/// Electrical role of a pin. ERC uses these to catch shorts that DRC
/// can't see (the geometry is legal, but the wiring is semantically
/// wrong — e.g. two outputs driving the same net).
///
/// Discretes (R, C, L, LED, D) are always `Passive` — they don't
/// drive or sink, just pass current. ICs declare a role per pin; the
/// default `Passive` is the safe fallback for anything the agent
/// doesn't classify.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PinRole {
    /// No active role — passes signal/current through. Default for
    /// resistors, capacitors, jumpers, and unspecified IC pins.
    #[default]
    Passive,
    /// Pin sinks a signal (e.g. UART RX, microcontroller GPIO in
    /// input mode). Needs at least one driver on its net.
    Input,
    /// Pin drives a signal (e.g. UART TX, level shifter output).
    /// Two `Output` pins on the same net is an electrical short.
    Output,
    /// Both — typical for I²C SDA/SCL, GPIO that toggles direction,
    /// data buses. ERC tolerates multiple `Bidir` on a net (they
    /// negotiate at protocol level).
    Bidir,
    /// Power source (regulator output, battery +, USB VBUS, header
    /// pin labelled +3V3 connected to a supply). Provides energy to
    /// the net.
    PowerOut,
    /// Power sink (chip VDD, MCU VBAT, decoupling cap on a rail).
    /// A net of `PowerIn` pins with no `PowerOut` source is the
    /// classic "forgot to connect the regulator" bug.
    PowerIn,
}

/// One pin on a generic-IC symbol. Discrete primitives (resistor,
/// capacitor…) define their pins implicitly via `SymbolKind`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchPin {
    pub number: String,
    /// Human-readable name (e.g. "VBAT", "PA0"). May be empty.
    pub name: String,
    pub side: PinSide,
    /// Electrical role for ERC. Defaults to `Passive` so existing
    /// schematics load with the loosest semantics — ERC won't fire
    /// drive-related rules until the agent classifies pins.
    #[serde(default)]
    pub role: PinRole,
}

/// What the symbol *is*. Determines body shape and implicit pinout for
/// discretes; carries the explicit pin list for generic ICs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SymbolKind {
    Resistor,
    Capacitor,
    Inductor,
    Led,
    Diode,
    GenericIc { pins: Vec<SchPin> },
}

impl SymbolKind {
    /// Pin definitions used by the renderer and by the connection
    /// validator. For discretes this is hard-coded so the agent does
    /// not have to repeat it on every call.
    #[must_use]
    pub fn pins(&self) -> Vec<SchPin> {
        match self {
            Self::Resistor | Self::Capacitor | Self::Inductor => vec![
                SchPin {
                    number: "1".into(),
                    name: String::new(),
                    side: PinSide::Left,
                    role: PinRole::Passive,
                },
                SchPin {
                    number: "2".into(),
                    name: String::new(),
                    side: PinSide::Right,
                    role: PinRole::Passive,
                },
            ],
            Self::Led | Self::Diode => vec![
                SchPin {
                    number: "A".into(),
                    name: "A".into(),
                    side: PinSide::Left,
                    role: PinRole::Passive,
                },
                SchPin {
                    number: "K".into(),
                    name: "K".into(),
                    side: PinSide::Right,
                    role: PinRole::Passive,
                },
            ],
            Self::GenericIc { pins } => pins.clone(),
        }
    }

    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Resistor => "R",
            Self::Capacitor => "C",
            Self::Inductor => "L",
            Self::Led => "LED",
            Self::Diode => "D",
            Self::GenericIc { .. } => "IC",
        }
    }
}

/// A symbol instance on the schematic page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Symbol {
    pub id: Id,
    pub reference: String,
    pub value: String,
    pub kind: SymbolKind,
    pub position: Point,
    pub rotation: f32,
    /// Optional library key the agent picked for this symbol — e.g.
    /// "`esp32_s3_zero`", "`lora_xl1262`", "`screw_term_2p_5.08mm`". When set,
    /// `palette.add_from_library` can spin up the matching footprint
    /// without the agent having to spell every pad geometry by hand.
    /// Always lowercase `snake_case` so lookups are deterministic.
    #[serde(default)]
    pub key: String,
    /// Free-form intent the agent records when creating the symbol —
    /// e.g. "ESP32-S3-Zero module; USB-C is on the short edge near
    /// pin 1, cable exits perpendicular to the pin rows". Persists in
    /// snapshots so the agent's future calls can recover its own
    /// reasoning without re-deriving it from the raw geometry.
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NetConnection {
    pub symbol_id: Id,
    pub pin_number: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Net {
    pub name: String,
    pub connections: Vec<NetConnection>,
    /// Optional name of the `NetClass` that governs this net's
    /// physical rules (trace width, clearance). `None` means "use the
    /// project's default — whatever `RouteOptions` / `DrcOptions`
    /// supply at the call site".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
}

impl Net {
    /// Whether this net's name uses bus / vector syntax like `D[0:7]`.
    /// Bus syntax expands at flatten / ERC time into individual nets
    /// (`D0`, `D1`, …, `D7`).
    #[must_use]
    pub fn is_bus(&self) -> bool {
        parse_bus_range(&self.name).is_some()
    }

    /// Expand a bus name like `D[0:7]` into the list of individual net
    /// names it covers. Returns `vec![name.clone()]` for non-bus names.
    #[must_use]
    pub fn expand_bus(&self) -> Vec<String> {
        match parse_bus_range(&self.name) {
            Some((prefix, lo, hi)) => (lo..=hi).map(|i| format!("{prefix}{i}")).collect(),
            None => vec![self.name.clone()],
        }
    }
}

/// Parse a bus name `PREFIX[LO:HI]` into `(prefix, lo, hi)`. Reversed
/// ranges (`[7:0]`) are accepted and normalised low → high. Anything
/// that doesn't match the syntax returns `None`.
fn parse_bus_range(name: &str) -> Option<(String, u32, u32)> {
    let open = name.rfind('[')?;
    let close = name.rfind(']')?;
    if close <= open + 1 || close != name.len() - 1 {
        return None;
    }
    let inner = &name[open + 1..close];
    let (lo_s, hi_s) = inner.split_once(':')?;
    let a: u32 = lo_s.trim().parse().ok()?;
    let b: u32 = hi_s.trim().parse().ok()?;
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    Some((name[..open].to_string(), lo, hi))
}

/// Heuristic: does this net name look like a power rail? Used by ERC's
/// `UnpoweredPowerNet` rule so a net called `+3V3` / `GND` / `VCC` is
/// treated as inherently powered even without a `PowerOut` pin or pour.
#[must_use]
pub fn is_power_named_net(name: &str) -> bool {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.starts_with('+') || trimmed.starts_with('-') {
        return true;
    }
    let upper = trimmed.to_ascii_uppercase();
    if upper == "GND" || upper.starts_with("GND_") || upper == "VSS" {
        return true;
    }
    if upper.starts_with("VCC") || upper.starts_with("VDD") || upper.starts_with("VBAT") {
        return true;
    }
    false
}

/// A named bundle of physical rules a net adheres to. Power rails
/// typically use a class with a wider `trace_width_mm`; high-speed
/// signals use one with tighter `clearance_mm`. Per-class fields
/// override the call-site defaults of the router and DRC; unset
/// fields fall back to the defaults so a class can override only what
/// it cares about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetClass {
    pub name: String,
    /// Trace width (mm) the router lays for nets in this class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_width_mm: Option<f64>,
    /// Minimum clearance (mm) between this net's copper and any
    /// foreign-net copper.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clearance_mm: Option<f64>,
    /// Via copper-pad diameter (mm) used when the router flips this
    /// net between layers. `None` falls back to `RouteOptions::via_diameter`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via_diameter_mm: Option<f64>,
    /// Via drill diameter (mm). `None` falls back to `RouteOptions::via_drill`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via_drill_mm: Option<f64>,
    /// Z0 impedance target in ohms (single-ended). DRC may warn when
    /// `trace_width_mm` doesn't match the stackup-derived Z0. Not yet
    /// consumed — surfaced for future high-speed work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_impedance_ohms: Option<f64>,
    /// Name of the partner net when this class defines one half of a
    /// differential pair. The router will eventually maintain
    /// `diff_gap_mm` edge-to-edge spacing between the pair; not yet
    /// consumed beyond schema persistence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_pair_with: Option<String>,
    /// Edge-to-edge gap (mm) between this trace and its `diff_pair_with`
    /// partner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_gap_mm: Option<f64>,
    /// Layers on which the schematic wants this class's nets to ride
    /// a copper pour instead of routed traces. `[Bottom]` is the
    /// classic GND-on-bottom pattern; `[Top, Bottom]` is the standard
    /// "GND plane on both layers" — every same-net pad on either
    /// layer connects via the pour without any routed trace. The
    /// `auto-pour` verb (and the `route` verb implicitly) materialise
    /// the listed pours. Empty = no pour, route as normal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pour_layers: Vec<CopperLayer>,
    /// Length-match target for nets in this class. If `Some(L)`, every
    /// net in the class is post-processed to end up close to L mm.
    /// Differential pair partners auto-derive their target from each
    /// other (longer partner's length becomes the shared target).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_length_mm: Option<f64>,
    /// Tolerance for length match (mm). Default 0.5 mm.
    #[serde(default = "default_length_tolerance_mm")]
    pub length_tolerance_mm: f64,
}

fn default_length_tolerance_mm() -> f64 {
    0.5
}

impl Default for NetClass {
    fn default() -> Self {
        Self {
            name: String::new(),
            trace_width_mm: None,
            clearance_mm: None,
            via_diameter_mm: None,
            via_drill_mm: None,
            target_impedance_ohms: None,
            diff_pair_with: None,
            diff_gap_mm: None,
            pour_layers: Vec::new(),
            target_length_mm: None,
            length_tolerance_mm: default_length_tolerance_mm(),
        }
    }
}

/// Static default class returned by `Schematic::class_for` when the
/// caller asks about a net that doesn't name a class. All fields are
/// `None` so callers fall back to their own defaults.
static DEFAULT_NET_CLASS: NetClass = NetClass {
    name: String::new(),
    trace_width_mm: None,
    clearance_mm: None,
    via_diameter_mm: None,
    via_drill_mm: None,
    target_impedance_ohms: None,
    diff_pair_with: None,
    diff_gap_mm: None,
    pour_layers: Vec::new(),
    target_length_mm: None,
    length_tolerance_mm: 0.5,
};

/// Effective routing rules for one net, with each field resolved to a
/// concrete `Length`. Built by `Schematic::resolved_for_net` from the
/// net's class (when set) with call-site fallbacks the caller passes in.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedNetRules {
    pub trace_width: Length,
    pub clearance: Length,
    pub via_diameter: Length,
    pub via_drill: Length,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Schematic {
    pub symbols: HashMap<Id, Symbol>,
    pub symbol_order: Vec<Id>,
    pub nets: HashMap<String, Net>,
    /// Named rule bundles. `nets[*].class` references entries here.
    /// Persists with the schematic so the router and DRC see the same
    /// classes the agent declared.
    #[serde(default)]
    pub net_classes: HashMap<String, NetClass>,
    /// Flat net-name → class-name map. New surface for Feature 1's net
    /// classes; coexists with the legacy `Net.class` per-net field, but
    /// the flat map wins when both are set (and works even before the
    /// net is declared via `connect`).
    #[serde(default)]
    pub net_to_class: HashMap<String, String>,
    /// Sub-sheets instantiated under this sheet. Each sheet has a
    /// reference label and its own nested `Schematic` (Feature 8). The
    /// router/DRC consume the flat view produced by [`Schematic::flatten`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sub_sheets: Vec<Sheet>,
    /// Ports this sheet exposes to its parent. Power/signal nets named
    /// here can be wired through a parent's `Sheet::port_bindings` to
    /// nets at the parent's namespace level.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<Port>,
}

/// One instance of a sub-sheet inside a parent schematic. The instance
/// has a reference (`PSU`, `RF`…) that prefixes every nested symbol's
/// reference in the flattened view, and a `port_bindings` map that
/// wires the sub-sheet's exposed `Port`s to nets at the parent level.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sheet {
    pub id: Id,
    /// Reference label like "PSU" — must be unique within the parent sheet.
    pub reference: String,
    /// Path to the sub-sheet within the project — currently a `Schematic`
    /// nested by value so the whole tree lives in one project file.
    /// Future work could swap this to a file path for multi-file projects.
    pub schematic: Schematic,
    /// Mapping from this sheet instance's port name → parent net name.
    /// A port unmapped here is dangling (ERC will warn).
    #[serde(default)]
    pub port_bindings: HashMap<String, String>,
}

/// Direction of a port as seen from inside the sub-sheet. Mirrors
/// `PinRole` semantics but applies to the sheet boundary itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortDirection {
    In,
    Out,
    Bidir,
    Power,
}

/// One port a sheet exposes to its parent. The `net` field is the
/// internal net name inside this sheet that the port wires to; the
/// parent binds the port to one of its own nets via
/// `Sheet::port_bindings`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Port {
    pub name: String,
    pub direction: PortDirection,
    pub net: String,
}

/// Flattened view of a hierarchical schematic. Every symbol's reference
/// is prefixed with its sheet path (e.g. `PSU/U1`), and every net in a
/// sub-sheet that's bound to a port is rewritten to the parent net it
/// resolves to. The router and DRC consume this flat view — they never
/// see the hierarchy.
#[derive(Debug, Clone, Default)]
pub struct FlatSchematic {
    /// Symbols keyed by their original id (unchanged), but with
    /// `reference` rewritten to include the sheet path prefix.
    pub symbols: HashMap<Id, Symbol>,
    /// Insertion order across the whole tree (parent first, then each
    /// child sheet in order).
    pub symbol_order: Vec<Id>,
    /// Nets keyed by the FLAT name — sub-sheet nets that map to a port
    /// bound to a parent net show up under the parent's name. Unbound
    /// ports stay prefixed with the sheet path.
    pub nets: HashMap<String, Net>,
    /// Net-class declarations from every sheet, merged. Sub-sheet
    /// classes can shadow same-named parent classes — the deepest
    /// declaration wins.
    pub net_classes: HashMap<String, NetClass>,
    /// Net → class mapping, merged across the tree with flat net names.
    pub net_to_class: HashMap<String, String>,
}

impl Schematic {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_symbol(&mut self, symbol: Symbol) -> Id {
        let id = symbol.id;
        self.symbol_order.push(id);
        self.symbols.insert(id, symbol);
        id
    }

    /// Look up a symbol by its reference designator. Returns the first
    /// match in insertion order (references are intended to be unique
    /// but we don't enforce that at the model level).
    #[must_use]
    pub fn find_by_reference(&self, reference: &str) -> Option<&Symbol> {
        self.symbol_order
            .iter()
            .filter_map(|id| self.symbols.get(id))
            .find(|s| s.reference == reference)
    }

    /// Add or replace the connections of a net. Replacing (rather than
    /// appending) makes the tool idempotent — calling `connect("VCC",
    /// [...])` twice yields the same state.
    pub fn set_net(&mut self, net: Net) {
        self.nets.insert(net.name.clone(), net);
    }

    pub fn symbols_in_order(&self) -> impl Iterator<Item = &Symbol> {
        self.symbol_order
            .iter()
            .filter_map(|id| self.symbols.get(id))
    }

    /// Add or replace a named net class.
    pub fn set_net_class(&mut self, class: NetClass) {
        self.net_classes.insert(class.name.clone(), class);
    }

    /// Assign `net_name` to the class `class_name`. Mirrors `Net::class`
    /// but lives in a flat map so callers that haven't yet declared the
    /// net can still record the assignment.
    pub fn assign_net_to_class(
        &mut self,
        net_name: impl Into<String>,
        class_name: impl Into<String>,
    ) {
        self.net_to_class.insert(net_name.into(), class_name.into());
    }

    /// Look up the class for the given net by name. Returns the class
    /// only if both (a) the net exists and has `class = Some(...)`,
    /// and (b) that class is in `net_classes`. Otherwise returns
    /// `None` — callers should fall back to their own defaults.
    #[must_use]
    pub fn class_for_net(&self, net_name: &str) -> Option<&NetClass> {
        // Prefer the flat `net_to_class` map (Feature 1) — it works
        // even when the net itself isn't declared yet. Fall back to the
        // per-`Net.class` legacy field for projects loaded from older
        // saves.
        if let Some(class_name) = self.net_to_class.get(net_name) {
            if let Some(c) = self.net_classes.get(class_name) {
                return Some(c);
            }
        }
        let class_name = self.nets.get(net_name)?.class.as_ref()?;
        self.net_classes.get(class_name)
    }

    /// Look up the class for `net_name`, or a static "empty default"
    /// class if no class is assigned / declared. Never returns `None`,
    /// so callers can chain `.field` accesses without a match.
    #[must_use]
    pub fn class_for(&self, net_name: &str) -> &NetClass {
        self.class_for_net(net_name).unwrap_or(&DEFAULT_NET_CLASS)
    }

    /// Resolve every routing rule for `net_name` to a concrete `Length`,
    /// preferring the net's class and falling back to the caller-supplied
    /// defaults. The four defaults mirror the fields of `RouteOptions`.
    #[must_use]
    pub fn resolved_for_net(
        &self,
        net_name: &str,
        default_trace_width: Length,
        default_clearance: Length,
        default_via_diameter: Length,
        default_via_drill: Length,
    ) -> ResolvedNetRules {
        let class = self.class_for(net_name);
        ResolvedNetRules {
            trace_width: class
                .trace_width_mm
                .map_or(default_trace_width, Length::from_mm),
            clearance: class
                .clearance_mm
                .map_or(default_clearance, Length::from_mm),
            via_diameter: class
                .via_diameter_mm
                .map_or(default_via_diameter, Length::from_mm),
            via_drill: class
                .via_drill_mm
                .map_or(default_via_drill, Length::from_mm),
        }
    }

    /// Add a sub-sheet instance to this schematic.
    pub fn add_sub_sheet(&mut self, sheet: Sheet) {
        self.sub_sheets.push(sheet);
    }

    /// Declare or replace a port on this sheet.
    pub fn set_port(&mut self, port: Port) {
        if let Some(existing) = self.ports.iter_mut().find(|p| p.name == port.name) {
            *existing = port;
        } else {
            self.ports.push(port);
        }
    }

    /// Flatten the sheet tree into a single namespace. Symbol references
    /// are prefixed by the sheet path (`PSU/U1`, `RF/AMP/Q1`); sub-sheet
    /// nets bound through ports are rewritten to the parent net they
    /// resolve to. The router and DRC consume the result. See
    /// [`FlatSchematic`] for the resolved field layout.
    #[must_use]
    pub fn flatten(&self) -> FlatSchematic {
        let mut out = FlatSchematic::default();
        flatten_into(self, "", &HashMap::new(), &mut out);
        out
    }

    /// All connections on a given pin, across nets. Each pin should
    /// belong to at most one net; if it appears in several, only the
    /// first is meaningful and the rest indicate a model bug.
    #[must_use]
    pub fn net_for_pin(&self, symbol_id: Id, pin_number: &str) -> Option<&str> {
        for net in self.nets.values() {
            for c in &net.connections {
                if c.symbol_id == symbol_id && c.pin_number == pin_number {
                    return Some(net.name.as_str());
                }
            }
        }
        None
    }
}

/// Helper: expand a (possibly bus-flavoured) net name into its
/// individual member nets. `D[0:7]` → `D0`..`D7`; plain names are
/// returned unchanged in a single-element vec.
fn expand_bus_name(name: &str) -> Vec<String> {
    if let Some((prefix, lo, hi)) = parse_bus_range(name) {
        (lo..=hi).map(|i| format!("{prefix}{i}")).collect()
    } else {
        vec![name.to_string()]
    }
}

/// Recursive worker for [`Schematic::flatten`]. `path` is the current
/// sheet path prefix (`""` at the root, `"PSU/"` one level down). The
/// `net_remap` map carries the parent-supplied rewrites: a sub-sheet's
/// internal net name → the parent's flat net name it should appear as.
fn flatten_into(
    sch: &Schematic,
    path: &str,
    net_remap: &HashMap<String, String>,
    out: &mut FlatSchematic,
) {
    let prefix_ref = |r: &str| -> String {
        if path.is_empty() {
            r.to_string()
        } else {
            format!("{path}{r}")
        }
    };
    let prefix_net = |n: &str| -> String {
        // Caller-supplied remap wins (port bindings).
        if let Some(parent) = net_remap.get(n) {
            return parent.clone();
        }
        if path.is_empty() {
            n.to_string()
        } else {
            format!("{path}{n}")
        }
    };

    // Symbols — rewrite the reference to include the sheet path.
    for id in &sch.symbol_order {
        let Some(sym) = sch.symbols.get(id) else {
            continue;
        };
        let mut cloned = sym.clone();
        cloned.reference = prefix_ref(&cloned.reference);
        out.symbols.insert(*id, cloned);
        out.symbol_order.push(*id);
    }

    // Nets — rewrite the name and remap any internal references that
    // collide with bound ports. Bus names like `D[0:7]` expand into
    // individual sub-nets so the router/DRC see flat names only.
    for (name, net) in &sch.nets {
        let flat_name = prefix_net(name);
        for expanded in expand_bus_name(&flat_name) {
            let entry = out.nets.entry(expanded.clone()).or_insert_with(|| Net {
                name: expanded.clone(),
                connections: Vec::new(),
                class: net.class.clone(),
            });
            for c in &net.connections {
                entry.connections.push(c.clone());
            }
            if entry.class.is_none() {
                entry.class.clone_from(&net.class);
            }
        }
    }

    // Net classes — last writer wins so child can shadow parent.
    for (k, v) in &sch.net_classes {
        out.net_classes.insert(k.clone(), v.clone());
    }
    for (net_name, class_name) in &sch.net_to_class {
        out.net_to_class
            .insert(prefix_net(net_name), class_name.clone());
    }

    // Recurse into sub-sheets, computing each child's prefix + remap.
    for sheet in &sch.sub_sheets {
        let child_path = if path.is_empty() {
            format!("{}/", sheet.reference)
        } else {
            format!("{path}{}/", sheet.reference)
        };
        // For each bound port: the sub-sheet's internal `port.net` name
        // should appear in the flat view as the parent's net it binds
        // to. Resolve the parent name through THIS level's remap (so a
        // chain of nested bindings propagates correctly).
        let mut child_remap: HashMap<String, String> = HashMap::new();
        for port in &sheet.schematic.ports {
            let Some(parent_net) = sheet.port_bindings.get(&port.name) else {
                continue;
            };
            let resolved = if let Some(higher) = net_remap.get(parent_net) {
                higher.clone()
            } else if path.is_empty() {
                parent_net.clone()
            } else {
                format!("{path}{parent_net}")
            };
            child_remap.insert(port.net.clone(), resolved);
        }
        flatten_into(&sheet.schematic, &child_path, &child_remap, out);
    }
}

#[cfg(test)]
mod hierarchy_tests {
    use super::*;
    use crate::board::Id;

    fn make_resistor(reference: &str) -> Symbol {
        Symbol {
            id: Id::new(),
            reference: reference.into(),
            value: "10k".into(),
            kind: SymbolKind::Resistor,
            position: crate::geometry::Point::ORIGIN,
            rotation: 0.0,
            key: String::new(),
            description: String::new(),
        }
    }

    #[test]
    fn sheet_flatten_prefixes_references() {
        // Parent has R0; child sheet "PSU" has R1, R2.
        let mut child = Schematic::new();
        let r1 = make_resistor("R1");
        let r2 = make_resistor("R2");
        let r1_id = r1.id;
        let r2_id = r2.id;
        child.add_symbol(r1);
        child.add_symbol(r2);
        child.set_net(Net {
            name: "VCC".into(),
            connections: vec![
                NetConnection {
                    symbol_id: r1_id,
                    pin_number: "1".into(),
                },
                NetConnection {
                    symbol_id: r2_id,
                    pin_number: "1".into(),
                },
            ],
            class: None,
        });

        let mut parent = Schematic::new();
        let r0 = make_resistor("R0");
        parent.add_symbol(r0);
        parent.add_sub_sheet(Sheet {
            id: Id::new(),
            reference: "PSU".into(),
            schematic: child,
            port_bindings: HashMap::new(),
        });

        let flat = parent.flatten();
        // Parent's R0 stays as "R0"; child's R1/R2 become "PSU/R1" / "PSU/R2".
        let refs: Vec<String> = flat
            .symbol_order
            .iter()
            .filter_map(|id| flat.symbols.get(id).map(|s| s.reference.clone()))
            .collect();
        assert!(refs.iter().any(|r| r == "R0"), "{refs:?}");
        assert!(refs.iter().any(|r| r == "PSU/R1"), "{refs:?}");
        assert!(refs.iter().any(|r| r == "PSU/R2"), "{refs:?}");
        // Net was unbound — stays prefixed.
        assert!(flat.nets.contains_key("PSU/VCC"), "{:?}", flat.nets.keys());
    }

    #[test]
    fn port_bound_to_parent_net_replaces_internal_name() {
        // Child has net "INTERNAL_3V3" wired to port "3V3" (power).
        // Parent binds port "3V3" → "+3V3". Flat net should be "+3V3".
        let mut child = Schematic::new();
        let r = make_resistor("R1");
        let r_id = r.id;
        child.add_symbol(r);
        child.set_net(Net {
            name: "INTERNAL_3V3".into(),
            connections: vec![NetConnection {
                symbol_id: r_id,
                pin_number: "1".into(),
            }],
            class: None,
        });
        child.set_port(Port {
            name: "3V3".into(),
            direction: PortDirection::Power,
            net: "INTERNAL_3V3".into(),
        });

        let mut parent = Schematic::new();
        let mut bindings = HashMap::new();
        bindings.insert("3V3".into(), "+3V3".into());
        parent.add_sub_sheet(Sheet {
            id: Id::new(),
            reference: "PSU".into(),
            schematic: child,
            port_bindings: bindings,
        });

        let flat = parent.flatten();
        assert!(
            flat.nets.contains_key("+3V3"),
            "expected +3V3, got {:?}",
            flat.nets.keys()
        );
        assert!(
            !flat.nets.contains_key("PSU/INTERNAL_3V3"),
            "internal name leaked: {:?}",
            flat.nets.keys()
        );
    }

    #[test]
    fn bus_expand_creates_individual_nets() {
        let net = Net {
            name: "D[0:7]".into(),
            connections: vec![],
            class: None,
        };
        assert!(net.is_bus());
        let expanded = net.expand_bus();
        assert_eq!(expanded.len(), 8);
        assert_eq!(expanded[0], "D0");
        assert_eq!(expanded[7], "D7");

        // Reverse form normalises to lo..=hi.
        let reversed = Net {
            name: "ADDR[3:0]".into(),
            connections: vec![],
            class: None,
        };
        let rev = reversed.expand_bus();
        assert_eq!(rev, vec!["ADDR0", "ADDR1", "ADDR2", "ADDR3"]);

        // Plain name passes through.
        let plain = Net {
            name: "VCC".into(),
            connections: vec![],
            class: None,
        };
        assert!(!plain.is_bus());
        assert_eq!(plain.expand_bus(), vec!["VCC".to_string()]);

        // Bus net inside a Schematic also expands at flatten time.
        let mut sch = Schematic::new();
        sch.set_net(Net {
            name: "D[0:1]".into(),
            connections: vec![],
            class: None,
        });
        let flat = sch.flatten();
        assert!(flat.nets.contains_key("D0"));
        assert!(flat.nets.contains_key("D1"));
        assert!(!flat.nets.contains_key("D[0:1]"));
    }

    #[test]
    fn unbound_port_kept_with_sheet_prefix() {
        // Child declares a port but parent doesn't bind it.
        let mut child = Schematic::new();
        let r = make_resistor("R1");
        let r_id = r.id;
        child.add_symbol(r);
        child.set_net(Net {
            name: "DANGLING".into(),
            connections: vec![NetConnection {
                symbol_id: r_id,
                pin_number: "1".into(),
            }],
            class: None,
        });
        child.set_port(Port {
            name: "OUT".into(),
            direction: PortDirection::Out,
            net: "DANGLING".into(),
        });

        let mut parent = Schematic::new();
        parent.add_sub_sheet(Sheet {
            id: Id::new(),
            reference: "RF".into(),
            schematic: child,
            port_bindings: HashMap::new(),
        });

        let flat = parent.flatten();
        assert!(
            flat.nets.contains_key("RF/DANGLING"),
            "{:?}",
            flat.nets.keys()
        );
    }
}

#[cfg(test)]
mod net_class_tests {
    use super::*;
    use crate::units::Length;

    #[test]
    fn class_lookup_uses_default_when_unassigned() {
        let sch = Schematic::new();
        let class = sch.class_for("FOO");
        assert!(class.name.is_empty());
        assert!(class.trace_width_mm.is_none());
        assert!(class.clearance_mm.is_none());
    }

    #[test]
    fn resolved_rules_inherit_from_options_when_class_silent() {
        let mut sch = Schematic::new();
        // Class with width set, clearance silent.
        sch.set_net_class(NetClass {
            name: "signals".into(),
            trace_width_mm: Some(0.30),
            ..NetClass::default()
        });
        sch.assign_net_to_class("DATA", "signals");

        let res = sch.resolved_for_net(
            "DATA",
            Length::from_mm(0.25),
            Length::from_mm(0.20),
            Length::from_mm(0.60),
            Length::from_mm(0.30),
        );
        // Class supplies trace width, clearance falls back to default.
        assert!((res.trace_width.to_mm() - 0.30).abs() < 1e-9);
        assert!((res.clearance.to_mm() - 0.20).abs() < 1e-9);
        assert!((res.via_diameter.to_mm() - 0.60).abs() < 1e-9);
        assert!((res.via_drill.to_mm() - 0.30).abs() < 1e-9);

        // Unrelated net falls back fully to options.
        let res2 = sch.resolved_for_net(
            "SOMETHING_ELSE",
            Length::from_mm(0.25),
            Length::from_mm(0.20),
            Length::from_mm(0.60),
            Length::from_mm(0.30),
        );
        assert!((res2.trace_width.to_mm() - 0.25).abs() < 1e-9);
    }
}
