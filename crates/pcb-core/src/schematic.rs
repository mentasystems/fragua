//! Schematic model.
//!
//! The schematic is a *netlist with layout hints*. The agent declares
//! symbols (by kind) and conectividad por nets — `R1.1` and `U1.8`
//! are both on net `VCC`, etc. Wires are not stored: rendering uses a
//! labels-only style (each pin stub carries its net name), which is a
//! valid KiCad convention and keeps the model identical to what the
//! agent reasons about.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::board::Id;
use crate::geometry::Point;

/// Side of a symbol body where a pin stub points outwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PinSide {
    Left,
    Right,
    Top,
    Bottom,
}

/// One pin on a generic-IC symbol. Discrete primitives (resistor,
/// capacitor…) define their pins implicitly via `SymbolKind`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchPin {
    pub number: String,
    /// Human-readable name (e.g. "VBAT", "PA0"). May be empty.
    pub name: String,
    pub side: PinSide,
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
                SchPin { number: "1".into(), name: String::new(), side: PinSide::Left },
                SchPin { number: "2".into(), name: String::new(), side: PinSide::Right },
            ],
            Self::Led | Self::Diode => vec![
                SchPin { number: "A".into(), name: "A".into(), side: PinSide::Left },
                SchPin { number: "K".into(), name: "K".into(), side: PinSide::Right },
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
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NetConnection {
    pub symbol_id: Id,
    pub pin_number: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Net {
    pub name: String,
    pub connections: Vec<NetConnection>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Schematic {
    pub symbols: HashMap<Id, Symbol>,
    pub symbol_order: Vec<Id>,
    pub nets: HashMap<String, Net>,
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
