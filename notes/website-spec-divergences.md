# Shortbread spec divergences

Known cases where elivagar diverges from the Shortbread 1.0 spec text.

## Railway zoom levels

The spec says for `rail` and `narrow_gauge` in the streets layer:

> "ways with `service=*` on zoom level 10+, other ways on zoom level 8+"

This means mainline rail (no `service` tag) should appear at z8, and service
tracks (sidings, yards) at z10. Both elivagar and Planetiler invert this:

| Feature | Spec | elivagar | Planetiler |
|---|---|---|---|
| `railway=rail` (mainline) | z8 | z10 | z10 |
| `railway=rail` + `service=*` | z10 | z8 | z8 |
| `narrow_gauge` (mainline) | z8 | z10 | z10 |
| `narrow_gauge` + `service=*` | z10 | z10 | z8 |

Two independent implementations arriving at the same inversion suggests the spec
wording is ambiguous. The phrase "ways with `service=*` on zoom level 10+" can be
read as either "service ways appear starting at z10" (spec intent) or "at z10+,
the service-tagged ones are included" (the reading both implementations used).

**Decision: match Planetiler.** The Shortbread schema exists for interoperability
between tile producers. Being the only implementation that differs at these zooms
would be a compatibility regression, not a fix. If the spec is updated to clarify,
elivagar will follow the clarification.
