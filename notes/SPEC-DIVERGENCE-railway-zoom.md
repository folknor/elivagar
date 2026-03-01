# Spec Divergence: Railway zoom levels

## Issue

The Shortbread spec 1.0 says for `rail` and `narrow_gauge` in the streets layer:

> "ways with `service=*` on zoom level 10+, other ways on zoom level 8+"

Mainline rail (no `service` tag) should be MORE prominent (z8), service tracks
(sidings, yards) LESS prominent (z10).

Both elivagar and Planetiler have it backwards: service tracks appear at z8,
mainline at z10.

## Comparison

| Feature | Spec min_zoom | elivagar | Planetiler |
|---|---|---|---|
| `railway=rail` (mainline) | **8** | 10 | 10 |
| `railway=rail` + `service=*` | **10** | 8 | 8 |
| `narrow_gauge` (mainline) | **8** | 10 | 10 |
| `narrow_gauge` + `service=*` | **10** | 10 (ok) | 8 |

## Analysis

Two independent implementations arriving at the same inversion suggests the spec
wording is ambiguous or counterintuitive. The phrase "ways with `service=*` on
zoom level 10+" can be read as either:

1. "Service ways appear starting at z10" (spec intent — service tracks are less
   prominent, shown later)
2. "At z10, service ways are the ones shown" (the reading both implementations
   apparently used)

## Decision

**Keep current behavior** (match Planetiler). Rationale:

- The whole point of the Shortbread schema is interoperability between producers.
  Being the only implementation that differs at these zooms is a compatibility
  regression, not a fix.
- Tilemaker's behavior is unknown — if it also matches the de facto standard, then
  "fixing" this would make elivagar the outlier.
- If the spec is updated to clarify, we should follow the clarification.

If we later find that Tilemaker follows the spec literally, this should be
reconsidered.

## Code reference

elivagar: `shortbread.rs`, `railway_zoom` function

```rust
"rail" => {
    if tags.has("service") { Some(8) }   // de facto: service rail at z8
    else                   { Some(10) }  // de facto: mainline rail at z10
}
"narrow_gauge" => Some(10),  // no service-tag split
```

Planetiler: `shortbread.yml`, min_zoom override blocks

```yaml
8:
  __all__:
    railway: [ rail, narrow_gauge ]
    service: __any__
10:
  __all__:
    railway: [ rail, narrow_gauge ]
    service: ''
```
