---
name: kicad-schematic
description: |
  Workflow skill for KiCAD schematic design via MCP tools. Triggers on: "design a circuit",
  "add a component", "wire up", "connect pins", "build schematic", "place resistor",
  "place cap", "place IC", "schematic", "add symbol", "net label", "power rail".
argument-hint: "[circuit description or task]"
---

# KiCAD Schematic Design Workflow

This skill guides Claude to design schematics using Konnect MCP tools.
ALL modifications go through MCP tools — never edit .kicad_sch files directly.

---

## Toolset Loading

Before any schematic work, load the required toolsets:

```
load_toolset('sch_components')   # place, move, rotate, delete symbols
load_toolset('sch_wiring')       # wires, net labels, power symbols, connections
load_toolset('sch_analysis')     # connection validation, short and orphan checks
load_toolset('sch_export')       # direct ERC and rendered schematic evidence
load_toolset('project')          # save_project before formal checks
```

Load additional toolsets as needed:

```
load_toolset('library')          # search_symbols, get_symbol_info, list_symbol_libraries
load_toolset('sch_batch')        # batch operations for 3+ items
```

Always call `get_active_toolsets()` first to see what is already loaded.

---

## Component Placement

Read [`references/common-lib-ids.md`](references/common-lib-ids.md) when choosing
a common generic KiCad symbol. It is a quick-start index, not an allowlist;
search the active libraries when the required part is absent or package-specific.

### Workflow

1. Search the library first: use `search_symbols` to find the correct lib_id
2. Get pin info: use `get_symbol_info` to see pin names, numbers, and positions
3. Place on the 1.27mm grid (KiCAD default schematic grid)
4. Verify placement with `list_schematic_components`

### Package-sensitive and custom parts

Before placing or wiring a custom symbol, a manufacturer-specific discrete,
or any package whose view can be mirrored, require the `kicad-library` skill's
**accepted physical pin map** for the exact MPN and package suffix. The map must
join each datasheet lead to the symbol pin and footprint pad, identify the
drawing view/direction, reconcile duplicate and mechanical pads, and include
query-back plus disposable rendered inspection. `get_symbol_info` proves the
library data that exists; it does not prove that data matches the package.

If the accepted physical pin map is missing, incomplete, based on a different
suffix, or ambiguous about top/bottom/mating view, stop before real schematic
placement. Do not infer physical numbering from a generic symbol name or from
the order pins appear on screen.

### Common Library IDs

| Component       | lib_id                         |
|-----------------|--------------------------------|
| Resistor        | `Device:R`                     |
| Capacitor       | `Device:C`                     |
| Capacitor Polar | `Device:C_Polarized`           |
| Inductor        | `Device:L`                     |
| LED             | `Device:LED`                   |
| Diode           | `Device:D`                     |
| Zener           | `Device:D_Zener`               |
| NPN Transistor  | `Transistor_BJT:Q_NPN_BEC`     |
| PNP Transistor  | `Transistor_BJT:Q_PNP_BEC`     |
| N-MOSFET        | `Transistor_FET:Q_NMOS_GDS`    |
| P-MOSFET        | `Transistor_FET:Q_PMOS_GDS`    |
| 2-pin Connector | `Connector_Generic:Conn_01x02` |
| 4-pin Connector | `Connector_Generic:Conn_01x04` |
| Ground          | `power:GND`                    |
| +3.3V           | `power:+3V3`                   |
| +5V             | `power:+5V`                    |
| VCC             | `power:VCC`                    |
| VDD             | `power:VDD`                    |

### Rotation Conventions

- 0 degrees: default orientation (pins left/right)
- 90 degrees: rotated CCW (useful for vertical components)
- 180 degrees: flipped horizontally
- 270 degrees: rotated CW

Power symbols connect by coincidence, not orientation, but the wrong rotation draws
the arrow back across the part it powers. The arrow always extends the *same*
direction the target pin already points — call `get_schematic_pin_locations` for its
`orientation_degrees` and look it up:

| Target pin's `orientation_degrees` | `GND` rotation | `VCC`/`VDD`/`+3V3`/`+5V` rotation |
|---|---|---|
| 0 (right) | 90  | 270 |
| 90 (up)   | 180 | 0   |
| 180 (left)| 270 | 90  |
| 270 (down)| 0   | 180 |

"GND uses 0" only holds for a downward-facing pin (an IC's bottom pin); "VCC uses 0"
only for an upward-facing one (a cap's top pin) — both common, but a pin facing left
or right (connectors, horizontal IC edges, rotated passives) needs a different
rotation from the table.

### Spacing Guidelines

- Between ICs: 30-50mm horizontal, 20-30mm vertical
- Between passive components: 10-15mm
- Between a decoupling cap and its IC: 5-10mm
- Leave room for wiring: minimum 5mm between component pins and other elements

---

## Wiring

Read [`references/wiring-patterns.md`](references/wiring-patterns.md) when
choosing between direct wires and labels or when building one of its common
subcircuits. Verify every named pin against the placed symbol before applying a
pattern.

### Connection Methods — Decision Table

| Scenario                                | Method                  | Why                                      |
|-----------------------------------------|-------------------------|------------------------------------------|
| Two pins physically close (<30mm)       | `connect_to_net` (shared net name) | Stub wire + net label, no route to check |
| Named signal (SDA, MOSI, EN, etc.)      | `connect_to_net`        | Stub wire + net label, cleaner           |
| Power rail (VCC, GND, +3V3)             | `add_power_symbol`      | Proper power symbol, global net          |
| Bus signals (D0-D7)                     | `connect_to_net`        | Net labels with bus naming               |
| Cross-sheet signal                      | Global label            | Connects across schematic sheets         |
| Multiple pins to same net (3+)          | `batch_connect_to_net`  | Efficient bulk operation                 |
| Two pins with a confirmed clear path    | `connect_pins` (opt-in — see warning) | Direct wire, auto-routed        |

### connect_pins

**Opt-in, not the default** — read this before reaching for it. The auto-router draws
a straight H+V(+H) path between the two pins with no check for what that path
crosses:

- If the route's bend or run shares an X or Y coordinate with a *third* component's
  pin in between, that pin joins the net silently — no warning, no error.
- If the two endpoints are two pins of the *same* symbol (e.g. a crystal's `XTAL1`
  and `XTAL2`), the "wire" is a dead short between them, and anything that was meant
  to sit electrically between those pins (the crystal itself, in that example) ends
  up in no net at all — Konnect treats a pin landing mid-span on a wire as connected;
  KiCad does not.

Use it only for a single, unambiguous point-to-point run where you have checked
the two components' other pins do not fall on the drawn path's X or Y range.
Afterwards run `find_shorted_nets` (`validate_wire_connections` /
`validate_component_connections` will not catch either failure — see Post-Placement
Verification below). Prefer `connect_to_net` for anything else, including two nearby
pins that just need to end up on the same net.

```
connect_pins(schematic, ref1, pin1, ref2, pin2)
```

- Specify pins by pin number (from get_schematic_pin_locations)
- Works best when pins are nearby and facing each other, with nothing between them
- Automatically creates wire segments with proper bends — unchecked ones

### connect_to_net

Use for named nets. Creates a short stub wire and attaches a net label.

```
connect_to_net(schematic, reference, pin_number, net)
```

- Preferred for signals that connect to 3+ pins
- Preferred for named buses and control signals
- Keeps schematic clean and readable
- Net name must be consistent across all connections
- Name the pin rather than passing `pin_x`/`pin_y`: the stub then points away
  from the symbol body on its own, instead of the label text running back
  across the pin names. Override with `direction` only to fix a layout clash.
- `batch_connect_to_net` does the same for many pins in one read/write, but places
  its labels directly on the pin endpoints with **no stub** — fine when neighbouring
  pins are well spaced, but on a tight pitch (a resistor/cap pair, adjacent header
  pins) the text overprints the neighbour. Its `pins` array takes
  `{reference, pin_number}` items under a top-level `net_name`; this is not the same
  shape as `batch_connect_pins`' `connections` array of `{ref1, pin1, ref2, pin2}` —
  check the tool schema rather than assuming one carries over to the other.
- On a tight pitch, prefer per-pin `connect_to_net` calls (its default 2.54 mm stub
  keeps the label clear) over `batch_connect_to_net`.
- Placing a label by hand with `add_schematic_net_label` instead? Take its
  rotation from `orientation_degrees` in `get_schematic_pin_locations`, or the
  text reads back across the symbol's pin names.

### add_power_symbol

Use for all power connections, in preference to labelling a pin with the rail name.

```
add_power_symbol(schematic, power_net, x, y, rotation?)
```

- Takes coordinates, not a reference and pin number. Place it on the pin
  endpoint (from `get_schematic_pin_locations`) — a power symbol carries its
  pin at its own origin, so the two coinciding is the connection.
- Landed straight on the pin, two power symbols on pins closer than ~5 mm apart
  (an IC's VDD/VSS pair, two pins of a passive) overprint each other's Value
  text. On that pitch, place the symbol a 2.54 mm stub away along the pin's
  outward direction instead and join it to the pin with `add_wire` — the same
  technique `connect_to_net` uses for its stub.
- `power_net` is loaded as `power:<power_net>`, so it must name a symbol in
  KiCad's power library: `+3V3` and `+12V`, never `3V3` or `12V`. A miss is an
  error and nothing is placed.
- `rotation` defaults to 0, which is only correct for one pin direction per
  symbol type — see Rotation Conventions above for the full table.
- A power pin landing mid-segment on a wire gets its junction dot
  automatically, in either order: symbol onto an existing wire, or a wire
  routed across an already-placed symbol.

---

## Batch Operations

Load `sch_batch` toolset when placing 3 or more components or making bulk connections.

### batch_place_components

Place multiple components in one call. Provide `schematic` and a `components` array of `{lib_id, x, y, rotation?, reference?, value?, unit?}` objects. Pass `reference` explicitly for each component -- it is not auto-assigned.

### batch_connect_to_net

Connect multiple pins to the same net in one call. Ideal for:
- Connecting all VCC pins on an IC
- Connecting all GND pins
- Bus signals across multiple ICs

### batch_edit_schematic_components

Bulk-modify component properties (values, footprints, fields) across multiple components.

### When to Use Batch vs Individual

- 1-2 components: individual calls
- 3+ components: batch operations
- Mixed operations (place + wire): do placement batch first, then wiring batch

---

## Common Patterns

### Decoupling Capacitor
Place 100nF cap (Device:C) within 5mm of IC power pin. Connect one pin to VCC via power symbol, other pin to GND via power symbol. One cap per VCC/VDD pin.

### Pull-up Resistor
Place resistor (Device:R) vertically. Connect one pin to the signal net via `connect_to_net`, other pin to VCC via `add_power_symbol`. Typical values: 4.7k for I2C, 10k for general.

### Voltage Divider
Two resistors in series, vertically aligned. Top to input net, middle junction to output net, bottom to GND. Use `connect_to_net` for input/output, `add_power_symbol` for GND.

### LED with Current-Limiting Resistor
Resistor in series with LED. Connect resistor to signal/power, resistor to LED anode, LED cathode to GND. R = (Vsupply - Vf) / If. Typical: 330R for 3.3V, 470R for 5V.

### Bypass/Decoupling Filter
For analog circuits: 100nF ceramic + 10uF electrolytic in parallel, close to power pins. Place ceramic closest to IC.

### Crystal Oscillator
Crystal (Device:Crystal) between XI and XO pins. Two load capacitors from each crystal pin to GND. Typical load caps: 12-22pF. Optional 1M feedback resistor across crystal.

---

## Post-Placement Verification

After placing components and wiring, always run these checks:

### annotate_schematic
Assigns reference designators (R1, C1, U1, etc.) to all unannotated components. Run after all placement is complete.

### validate_wire_connections
Checks that all wires connect properly to pins. Reports:
- Dangling wire ends
- Wires that miss pins
- Overlapping wires

**Does not check which net a pin ends up on** — a pin merged onto the wrong net by
a `connect_pins` collision still "has a wire" and reports clean. Use
`export_netlist_summary` or `find_shorted_nets` for net-level correctness.

### validate_component_connections
Verifies that components have the expected connections. Reports:
- Unconnected pins that should be connected
- Missing power connections

Same limitation as above: it checks that a pin touches *something*, not which net
that something belongs to.

### find_orphan_items
Finds floating wires, labels, and symbols that are not connected to anything.

### Verification Workflow

1. Place and wire complete functional blocks.
2. Run `annotate_schematic`, then save with `save_project`.
3. Run `validate_wire_connections` and `validate_component_connections` — pin-level
   only (see the caveats above).
4. Run `export_netlist_summary` and `find_shorted_nets`; reconcile every net against
   what was intended. This is the net-level check the validators above cannot do,
   and the only thing that catches a `connect_pins` collision or a same-symbol short.
5. Run `find_orphan_items` as a heuristic and corroborate its findings.
6. Run direct KiCad ERC with `run_erc` and classify every violation.
7. Run `render_schematic_png` with inline output and inspect the actual image — text
   overlap, a label crowding a pin, and a wrongly-rotated power symbol are only
   visible here; no connectivity check sees layout.
8. Fix findings and repeat every check invalidated by the edits.

---

## Visual feedback loop

The agent can see its own schematic. After meaningful edits:

1. `render_schematic_png` — rasterize the sheet (pass `inline` true to get
   the image back as base64 and actually look at it).
2. `set_visual_baseline` — capture the known-good render before a batch of
   edits (stored under the project's own state directory with the source
   hash and renderer identity).
3. `compare_visual_baseline` — after edits: PASS/DRIFT against a 2% content
   threshold with the changed region's bounding box. "No baseline stored" is
   a normal state, and a baseline from an older renderer is flagged rather
   than silently trusted.

Use the loop to catch what connectivity checks cannot. Completion requires
coherent functional grouping, label-inclusive overlap inspection, clear signal
flow, and page-boundary acceptance for every symbol, label, and note. Inspect
the image itself; a successful render command is not visual acceptance.

## Evidence and completion gate

Apply this order when evidence disagrees:

1. Exact requirements and manufacturer datasheets.
2. Direct KiCad ERC and saved/exported connectivity.
3. Direct net, short, pin, and component evidence from Konnect.
4. Aggregate review results.
5. Heuristic orphan, single-pin, decoupling, and best-practice findings.

A weaker heuristic may raise a question but does not override stronger direct
evidence. If any required check did not run, failed structurally, returned
impossible coverage, or contradicts stronger evidence without resolution, the
result is `INCOMPLETE`. Report the blocked evidence and stop short of a clean or
production-ready claim.

## Rules

1. **Never edit .kicad_sch files directly** — all changes go through MCP tools
2. **Never guess pin numbers** — always use `get_schematic_pin_locations` or `get_symbol_info` to look up pin numbers before connecting
3. **Always verify after changes** — run validation tools after placing and wiring;
   they check pins, not nets, so follow up with `export_netlist_summary` /
   `find_shorted_nets` for net-level correctness
4. **Use the grid** — all placements on 1.27mm grid
5. **Search before placing** — use `search_symbols` to confirm lib_id exists
6. **Power symbols for power** — use `add_power_symbol` for rails, not net labels
7. **Net labels for named signals** — keeps schematics readable
8. **Save frequently** — call `save_project` after major operations
9. **Load toolsets first** — check `get_active_toolsets()` and load what you need before starting
10. **Batch for bulk** — use batch toolset for 3+ repetitive operations
