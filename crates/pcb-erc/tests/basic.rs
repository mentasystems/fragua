//! Smoke tests for each ERC rule: build a small synthetic schematic
//! that triggers exactly one violation and verify it fires.

use pcb_core::schematic::{Net, NetConnection, PinRole, PinSide, SchPin, Symbol, SymbolKind};
use pcb_core::{Board, CopperLayer, Footprint, Id, Length, Pad, Point, Schematic};
use pcb_erc::{run, ErcKind, ErcOptions};

fn ic_symbol(reference: &str, pin_count: usize) -> Symbol {
    ic_symbol_with_roles(reference, &(1..=pin_count).map(|_| PinRole::Passive).collect::<Vec<_>>())
}

fn ic_symbol_with_roles(reference: &str, roles: &[PinRole]) -> Symbol {
    let pins: Vec<SchPin> = roles
        .iter()
        .enumerate()
        .map(|(i, role)| SchPin {
            number: (i + 1).to_string(),
            name: String::new(),
            side: PinSide::Left,
            role: *role,
        })
        .collect();
    Symbol {
        id: Id::new(),
        reference: reference.into(),
        value: String::new(),
        kind: SymbolKind::GenericIc { pins },
        position: Point::new(Length::ZERO, Length::ZERO),
        rotation: 0.0,
        key: String::new(),
        description: String::new(),
    }
}

fn empty_board() -> Board {
    Board::new()
}

fn board_with_phantom_pad(net: &str) -> Board {
    let mut board = Board::new();
    board.add_footprint(Footprint {
        id: Id::new(),
        reference: "R1".into(),
        value: String::new(),
        library: "demo".into(),
        position: Point::new(Length::from_mm(10.0), Length::from_mm(10.0)),
        rotation: 0.0,
        layer: CopperLayer::Top,
        pads: vec![Pad {
            number: "1".into(),
            name: String::new(),
            offset: Point::new(Length::ZERO, Length::ZERO),
            size: (Length::from_mm(1.0), Length::from_mm(1.0)),
            layer: CopperLayer::Top,
            net: Some(net.to_string()),
        }],
        key: String::new(),
        description: String::new(),
        edge_mounted: false,
        silk: Vec::new(),
    });
    board
}

#[test]
fn floating_pin_fires_when_pin_is_unconnected() {
    let mut sch = Schematic::new();
    let u = ic_symbol("U1", 3);
    let u_id = u.id;
    sch.add_symbol(u);
    // Wire pin 1 only — pins 2 and 3 are floating.
    sch.set_net(Net {
        name: "S".into(),
        connections: vec![NetConnection {
            symbol_id: u_id,
            pin_number: "1".into(),
        }, NetConnection {
            // Add a second endpoint so the FloatingNet rule doesn't
            // fire and confuse the test.
            symbol_id: u_id,
            pin_number: "1".into(),
        }],
        class: None,
    });
    let report = run(&empty_board(), &sch, &ErcOptions::default());
    let floating: Vec<&pcb_erc::Violation> = report
        .violations
        .iter()
        .filter(|v| v.kind == ErcKind::FloatingPin)
        .collect();
    assert_eq!(floating.len(), 2, "expected pin 2 and pin 3 floating, got {floating:?}");
}

#[test]
fn floating_net_fires_when_only_one_pin() {
    let mut sch = Schematic::new();
    let u = ic_symbol("U1", 2);
    let u_id = u.id;
    sch.add_symbol(u);
    sch.set_net(Net {
        name: "S".into(),
        connections: vec![NetConnection {
            symbol_id: u_id,
            pin_number: "1".into(),
        }],
        class: None,
    });
    let report = run(&empty_board(), &sch, &ErcOptions::default());
    assert!(report
        .violations
        .iter()
        .any(|v| v.kind == ErcKind::FloatingNet && v.involved.contains(&"S".to_string())));
}

#[test]
fn duplicate_pin_fires_when_same_pin_in_two_nets() {
    let mut sch = Schematic::new();
    let u = ic_symbol("U1", 2);
    let u_id = u.id;
    sch.add_symbol(u);
    sch.set_net(Net {
        name: "A".into(),
        connections: vec![NetConnection { symbol_id: u_id, pin_number: "1".into() }],
        class: None,
    });
    sch.set_net(Net {
        name: "B".into(),
        connections: vec![NetConnection { symbol_id: u_id, pin_number: "1".into() }],
        class: None,
    });
    let report = run(&empty_board(), &sch, &ErcOptions::default());
    let dup = report
        .violations
        .iter()
        .find(|v| v.kind == ErcKind::DuplicatePin)
        .expect("expected DuplicatePin");
    assert_eq!(dup.severity, pcb_erc::Severity::Error);
    assert!(dup.involved.contains(&"U1.1".to_string()));
}

#[test]
fn empty_net_fires_when_no_connections() {
    let mut sch = Schematic::new();
    sch.set_net(Net {
        name: "ORPHAN".into(),
        connections: Vec::new(),
        class: None,
    });
    let report = run(&empty_board(), &sch, &ErcOptions::default());
    assert!(report
        .violations
        .iter()
        .any(|v| v.kind == ErcKind::EmptyNet && v.involved.contains(&"ORPHAN".to_string())));
}

#[test]
fn orphan_symbol_fires_when_no_pin_is_in_any_net() {
    let mut sch = Schematic::new();
    sch.add_symbol(ic_symbol("U1", 4));
    let report = run(&empty_board(), &sch, &ErcOptions::default());
    assert!(report
        .violations
        .iter()
        .any(|v| v.kind == ErcKind::OrphanSymbol && v.involved.contains(&"U1".to_string())));
}

#[test]
fn phantom_net_fires_when_pad_references_unknown_net() {
    let board = board_with_phantom_pad("MYSTERY");
    let sch = Schematic::new();
    let report = run(&board, &sch, &ErcOptions::default());
    let phantom = report
        .violations
        .iter()
        .find(|v| v.kind == ErcKind::PhantomNet)
        .expect("expected PhantomNet");
    assert_eq!(phantom.severity, pcb_erc::Severity::Error);
    assert!(phantom.involved.iter().any(|s| s == "MYSTERY"));
}

#[test]
fn multiple_drivers_fires_when_two_outputs_share_a_net() {
    let mut sch = Schematic::new();
    let u1 = ic_symbol_with_roles("U1", &[PinRole::Output, PinRole::Passive]);
    let u2 = ic_symbol_with_roles("U2", &[PinRole::Output, PinRole::Passive]);
    let u1_id = u1.id;
    let u2_id = u2.id;
    sch.add_symbol(u1);
    sch.add_symbol(u2);
    sch.set_net(Net {
        name: "S".into(),
        connections: vec![
            NetConnection { symbol_id: u1_id, pin_number: "1".into() },
            NetConnection { symbol_id: u2_id, pin_number: "1".into() },
        ],
        class: None,
    });
    let report = run(&Board::new(), &sch, &ErcOptions::default());
    let drivers = report
        .violations
        .iter()
        .find(|v| v.kind == ErcKind::MultipleDrivers)
        .expect("expected MultipleDrivers");
    assert_eq!(drivers.severity, pcb_erc::Severity::Error);
    assert!(drivers.involved.contains(&"U1.1".to_string()));
    assert!(drivers.involved.contains(&"U2.1".to_string()));
}

#[test]
fn unpowered_power_net_fires_when_only_consumers() {
    let mut sch = Schematic::new();
    let u = ic_symbol_with_roles("U1", &[PinRole::PowerIn]);
    let u_id = u.id;
    sch.add_symbol(u);
    sch.set_net(Net {
        name: "+3V3".into(),
        connections: vec![NetConnection { symbol_id: u_id, pin_number: "1".into() }],
        class: None,
    });
    let report = run(&Board::new(), &sch, &ErcOptions::default());
    assert!(report
        .violations
        .iter()
        .any(|v| v.kind == ErcKind::UnpoweredPowerNet));
}

#[test]
fn unpowered_power_net_silent_when_pour_supplies_it() {
    use pcb_core::{CopperLayer, Pour};
    let mut sch = Schematic::new();
    let u = ic_symbol_with_roles("U1", &[PinRole::PowerIn]);
    let u_id = u.id;
    sch.add_symbol(u);
    sch.set_net(Net {
        name: "GND".into(),
        connections: vec![NetConnection { symbol_id: u_id, pin_number: "1".into() }],
        class: None,
    });
    let mut board = Board::new();
    board.add_pour(Pour { net: "GND".into(), layer: CopperLayer::Bottom });
    let report = run(&board, &sch, &ErcOptions::default());
    assert!(!report
        .violations
        .iter()
        .any(|v| v.kind == ErcKind::UnpoweredPowerNet));
}

#[test]
fn unconnected_input_fires_when_no_driver() {
    let mut sch = Schematic::new();
    let u1 = ic_symbol_with_roles("U1", &[PinRole::Input]);
    let u2 = ic_symbol_with_roles("U2", &[PinRole::Passive]);
    let u1_id = u1.id;
    let u2_id = u2.id;
    sch.add_symbol(u1);
    sch.add_symbol(u2);
    sch.set_net(Net {
        name: "S".into(),
        connections: vec![
            NetConnection { symbol_id: u1_id, pin_number: "1".into() },
            NetConnection { symbol_id: u2_id, pin_number: "1".into() },
        ],
        class: None,
    });
    let report = run(&Board::new(), &sch, &ErcOptions::default());
    assert!(report
        .violations
        .iter()
        .any(|v| v.kind == ErcKind::UnconnectedInput));
}

#[test]
fn clean_schematic_produces_zero_violations() {
    let mut sch = Schematic::new();
    let u1 = ic_symbol("U1", 2);
    let u2 = ic_symbol("U2", 2);
    let (u1_id, u2_id) = (u1.id, u2.id);
    sch.add_symbol(u1);
    sch.add_symbol(u2);
    // Two nets, both with two endpoints, every pin covered.
    sch.set_net(Net {
        name: "A".into(),
        connections: vec![
            NetConnection { symbol_id: u1_id, pin_number: "1".into() },
            NetConnection { symbol_id: u2_id, pin_number: "1".into() },
        ],
        class: None,
    });
    sch.set_net(Net {
        name: "B".into(),
        connections: vec![
            NetConnection { symbol_id: u1_id, pin_number: "2".into() },
            NetConnection { symbol_id: u2_id, pin_number: "2".into() },
        ],
        class: None,
    });
    let report = run(&empty_board(), &sch, &ErcOptions::default());
    assert_eq!(report.error_count, 0, "{:?}", report.violations);
    assert_eq!(report.warning_count, 0, "{:?}", report.violations);
}
