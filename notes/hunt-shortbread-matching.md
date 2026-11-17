I would rewrite the matcher and its input/output boundary. The strongest opportunity is a block-aware classifier that reads each element's tags once, derives all Shortbread decisions together, and produces compact output recipes. Moving that classifier into PBF decode workers is the stronger end-to-end design.

Changing individual string searches would leave most of the structural waste intact. These recommendations come from source inspection; I made no changes and ran no builds, tests, or workloads.

**1. Replace independent layer matching with one compiled classification pass. High conviction, architectural rewrite.**

The current execution model repeatedly asks questions of the same tag list. [`Tags`](/home/folk/Programs/elivagar/crates/lib/src/shortbread/mod.rs:594) implements every lookup as a linear scan, and [`match_element`](/home/folk/Programs/elivagar/crates/lib/src/shortbread/mod.rs:852) invokes every geometry-compatible layer independently.

Street matching makes the problem particularly clear. [`street_match`](/home/folk/Programs/elivagar/crates/lib/src/shortbread/streets.rs:140) has 31 ordered key/value rows. `first_tag_match` searches the element's tags separately for each row. An element without any street classification therefore answers 31 negative queries. Eligible named or referenced elements subsequently encounter another 31-row table for street labels.

This cost also falls on buildings and other ways that never produce a street feature. Sites and public transport add their own nine-row searches. Other duplication includes:

- Water geometry and water labels independently classify the same water tags.
- Street attributes and paint rank independently recompute bridge and tunnel predicates.
- Address exclusion calls `would_match_poi`, which constructs and discards category attributes. The POI layer then repeats classification and attribute construction. This happens even before the address matcher checks whether an address exists. See [`would_match_poi`](/home/folk/Programs/elivagar/crates/lib/src/shortbread/pois.rs:36) and [`match_addresses`](/home/folk/Programs/elivagar/crates/lib/src/shortbread/land.rs:340).
- Common name lookups and copies repeat across matched layers.

The rewrite should change the direction of evaluation: tags contribute facts once; those facts determine every output.

For each PBF block, maintain a compact dictionary indexed by its string-table IDs. Classify referenced keys into a fixed Shortbread key vocabulary and relevant values into categorical tokens. Unknown values retain their original string-table reference for passthrough attributes. Each element then makes one pass over integer key/value pairs, accumulating:

- Presence and first-value references for relevant keys.
- Recognized key/value predicates.
- Classification candidates with explicit precedence.
- Shared flags such as bridge, tunnel and oneway.
- References to dynamic attribute values.

A small, profile-specific decision program turns those facts into all outputs. A street classification can supply geometry kind, label kind, their separate zoom rules, rail/link status and paint rank. A POI selector returns a category token used both for address exclusion and POI emission; constructing attributes is a later operation.

This should be compiled direct code or compact static decisions. A generic expression interpreter would introduce another runtime traversal.

The reader already supplies the essential public mechanisms: `raw_tags()` exists for nodes, dense nodes, ways and relations; [`PrimitiveBlock::string_table_entry`](/home/folk/Programs/elivagar/research/pbfhogg/src/read/block.rs:672) exposes their dictionary. Elivagar currently expands tags into string pairs instead.

The payoff is plausible because this removes work from the common negative path across essentially every tagged element. It changes repeated tag-list scans into one pass plus small integer decisions. Vocabulary recognition becomes amortized across elements sharing a block.

The main risks are semantic fidelity and dictionary overhead. Avoid eagerly doing expensive value classification for every unique name or unrelated string. Cache classification when an ID is used in a relevant role. Keep that cache local to a block or worker; a global interner would add synchronization and retain input-sized state.

There are several important semantic distinctions:

- `get` uses the first occurrence of a key; `has_value` and `has_any` inspect any occurrence. One stored value per key cannot reproduce both.
- Row priority decides the winner independently of tag order. An unrecognized higher-priority value permits a later candidate.
- Street geometry and street labels have different precedence tables. Sharing decoded facts must preserve both selectors.
- Presence, an empty value, an unknown value and absence are distinct.
- Outputs are a sequence, not simply a layer bitmask. A closed way can produce both polygon and cliff-line features in `Land`.

These are manageable requirements for a compiled classifier. They are reasons to define its facts precisely, not reasons to retain repeated searches.

**2. Replace owned attribute trees with compact output recipes. High conviction, architectural rewrite coupled to emission.**

The current matcher materializes an expensive intermediate representation before emission serializes it again.

[`LayerMatch`](/home/folk/Programs/elivagar/crates/lib/src/shortbread/mod.rs:710) is explicitly asserted to occupy 280 bytes. Every match carries inline space for eight generic attributes, including a match that contains only one constant kind. `Hits` reserves four such entries plus pending-rule state. `Hits::finish` then collects another match vector.

More importantly, [`attr_dyn`](/home/folk/Programs/elivagar/crates/lib/src/shortbread/mod.rs:1020) allocates a `String` for every dynamic attribute. The lifetime requirement of prepared relations has effectively become the representation used for every node and way, even though their emission occurs while the PBF block remains alive.

There are additional allocations before matching: tagged nodes collect a fresh `Vec<(&str, &str)>`, and ways do the same. Neither collection is necessary with the classifier above.

I would replace this with an element-level plan containing compact output descriptors and shared attribute inputs:

- Constant kinds and categorical POI values refer to static recipes.
- Booleans occupy flags, with their visibility rules supplied by the recipe.
- Dynamic strings refer to the block dictionary while the block remains alive.
- Parsed numbers occupy compact typed storage only when needed.
- Each output descriptor supplies its layer, geometry policy, zoom and rank.
- A pending boundary-area operation is recorded separately.

Emission should serialize directly from these recipes into its existing attribute byte buffers. That removes the path from borrowed PBF text to owned strings to generic attributes to serialized bytes.

For asynchronous relation work, copy selected attribute data into a compact owned arena or serialized attribute block once. Do not retain a complete decompressed PBF block merely to keep a few string references alive.

This also exposes a useful emission-side improvement: serialize attributes by distinct visibility interval. Most layers have one attribute set throughout their emitted zoom range. Streets have up to three sets, with transitions at z11 and z14.

Currently point and line emission call `encode_attrs_bytes` at each zoom. Polygon emission caches per zoom, and resets that cache for each geometry emission. It already avoids serialization per tile; the remaining opportunity is to share identical attributes across zooms and multipolygon components. See [`encode_attrs_bytes`](/home/folk/Programs/elivagar/crates/lib/src/wire_format.rs:257) and the [polygon attribute cache](/home/folk/Programs/elivagar/crates/lib/src/pipeline/emit.rs:1093).

The likely benefits are lower allocation traffic, less repeated copying and smaller queued match state. I would expect the throughput benefit to be more broadly relevant than the peak-memory benefit: geometry, sort buffers and indexes can still dominate peak RSS.

The emission scope needs to participate. Keeping an adapter that eagerly reconstructs the old `LayerMatch` objects would surrender much of the gain.

Preserving existing record bytes is feasible if serialization retains attribute order, types, visibility and string treatment. If the sort wire format also changes, ordering needs explicit attention: [`record_cmp`](/home/folk/Programs/elivagar/crates/lib/src/sort/key.rs:153) breaks sort-key ties using payload bytes. A different internal representation can therefore change feature order even when its decoded attributes are equivalent. Preserve a canonical semantic tie-breaker rather than comparing transient dictionary or arena IDs.

**3. Perform classification during decode, and extract relation work once. Architectural rewrite with the strongest potential to improve serialized throughput.**

The first two changes remove unnecessary work. This change also removes that work from ownership bottlenecks.

On raw input, one node worker owns the node store and performs tagged-node matching and emission. Ordered coordinate insertion requires sequencing; tag classification does not. In the relation tail, matching happens inside `prepare_relation`, before prepared relations cross `par_bridge` into parallel processing. Consequently, matching and its allocations contribute to serialized production there.

pbfhogg already contains relevant internal machinery:

- [`for_each_fused_block` and `for_each_fused_buffer`](/home/folk/Programs/elivagar/research/pbfhogg/src/read/reader.rs:364) execute transforms on decode workers.
- [`DecodeTransform`](/home/folk/Programs/elivagar/research/pbfhogg/src/read/pipeline.rs:215) allows compact transformed results to cross to the consumer.
- [`BlockBuffer::project` and `GroupProjection`](/home/folk/Programs/elivagar/research/pbfhogg/src/read/block.rs:1135) extract data during validation without requiring a subsequent full element walk.
- The [tags-count implementation](/home/folk/Programs/elivagar/research/pbfhogg/src/commands/tags_count/tally.rs:43) demonstrates recording tag-array positions through a projection and consuming raw IDs after validation.

These are currently crate-private, so the full change requires pbfhogg work. That is a suitable boundary to break.

The coherent design is:

- Decode workers classify tags and construct compact feature plans.
- Raw-node processing returns ordered coordinate batches for the store, while independent feature work runs in parallel.
- Way processing receives classification facts before expensive feature-specific geometry work. It resolves geometry once and chooses the applicable outputs.
- Relation ingestion produces compact requests containing selected attributes, pending area policy and member IDs/roles. Geometry workers later resolve and process those requests.

Way handling particularly benefits from separating classification from output construction. [`process_planned_way_into`](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/ways.rs:536) currently probes closed-way matching, sometimes probes open-way matching, resolves coordinates, and may invoke matching again for the actual geometry type. This can construct attributes for an interpretation that is discarded.

Compute facts once and retain geometry applicability in the output recipes. The eventual closure decision must remain based on resolved coordinates: missing references and coordinate equality mean endpoint node IDs alone are insufficient.

The stronger relation redesign is to make the existing prepass the single relation-ingestion pass. Today it performs full matching merely to decide which member ways are needed, discards those matches, and later [`prepare_relation`](/home/folk/Programs/elivagar/crates/lib/src/pipeline/relations.rs:45) matches again. Meanwhile, the main read buffers decompressed relation blocks or records ranges for replay.

The prepass should instead return boundary membership plus compact relation requests. Those requests replace raw relation-block buffering and later tag matching. A bounded compact spool is reasonable if necessary; storing the relevant relation representation is preferable to retaining and re-decoding its original PBF container.

This also lets geometry resolution move onto relation workers. The present [`process_relation_blocks`](/home/folk/Programs/elivagar/crates/lib/src/pipeline/relations.rs:248) eagerly resolves every relation in one block into a `Vec` before handing its items through `par_bridge`.

An adjacent opportunity is visible here: under injected membership, Elivagar computes the exact matched-relation `needed_ways` set and then discards it in favor of the broader injected bitmap. See [`start_way_phase`](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/mod.rs:332). Coordinating compact relation requests with exact dependencies could avoid unnecessary way-index storage for unmatched relations. That belongs with the phase12 and index scope.

The payoff depends on the workload's actual critical path. Removing matching from the raw-node worker and relation producer can improve wall time more than accelerating already-parallel way matching. Eliminating repeated relation extraction also reduces materialization and replay.

The risks are broader than in the classifier itself:

- Keep coordinate-store sequencing without imposing that sequencing on independent features.
- Preserve relation-member geometry and boundary-derived matches, including ways with no useful tags.
- Preserve extent collection and shared-node pin analysis. Unrendered ways can still influence rendered geometry.
- Bound retained plans by bytes; compact plans can still accumulate.
- Publish no output from a block that subsequently fails validation.

Dense nodes offer another structural saving. Their tags are currently walked during validation, walked again to locate each node's tag slice, and decoded again through the tag iterator. A fused reader path can classify while consuming that stream. The existing projection hook runs after dense-tag validation, so merely adopting the hook does not eliminate every pass; removing them requires changing the decoder itself.

These three opportunities are parts of one replacement. Their estimated gains must not be added together as independent savings.

**The area rule is already appropriately lazy.**

[`Hits::finish`](/home/folk/Programs/elivagar/crates/lib/src/shortbread/mod.rs:767) measures area at most once, only when a pending rule requires it. The only pending rule is boundary labels. Buildings, water and ordinary land matching do not trigger this measurement.

The expensive operation is [`area_sq_meters`](/home/folk/Programs/elivagar/crates/lib/src/geometry/polygon.rs:273), which evaluates `cosh` per edge. Large administrative boundaries can make that significant, but replacing it with projected shoelace area or an approximation changes the emitted `way_area` and potentially zoom thresholds.

I would retain the existing arithmetic in the architectural rewrite. Compute it over finalized geometry when requested. Folding the same accumulation into an unavoidable geometry traversal is possible afterwards, but retaining summation order matters for identical floating-point attributes. This is a narrower opportunity than removing the matching and ownership machinery around it.

**Several local changes are worthwhile, but have a lower ceiling.**

| Local change | Concrete saving | Assessment |
|---|---|---|
| Share one POI category decision between addresses and POIs | Removes duplicate searches, discarded attributes and sometimes repeated numeric parsing | Strongest isolated matcher fix |
| Replace row-by-row searches with one lookup per key and direct value dispatch | Removes repeated street, site and transport scans | Material local improvement, though subsumed by the classifier |
| Make common numeric parsing allocation-free | `parse_length_m` lowercases into an owned string; decimal normalization allocates even without commas | Medium value on building-heavy inputs |
| Transform street refs once | `replace` creates a string and `attr_dyn` copies it again | Useful allocation reduction |
| Skip unused dense-node metadata | Node workers use `for_each_element`, while `elements_skip_metadata` already exists | Useful reader-side change |

Numeric parsing is in [`osm_value.rs`](/home/folk/Programs/elivagar/crates/lib/src/shortbread/osm_value.rs:11). Borrow the ordinary numeric token, compare unit suffixes without allocating lowercase text, and normalize exceptional comma forms into reusable scratch. A lazy block-local cache can reuse parsed values, distinguished by parsing policy. Integer, count and length parsing intentionally have different semantics.

Street refs need their current Unicode character counts, newline behavior and empty-reference handling preserved; the duplicate allocation is visible in [`match_street_labels_line`](/home/folk/Programs/elivagar/crates/lib/src/shortbread/streets.rs:312).

Changing POI membership arrays to a different container, tuning inline capacities, or rearranging enum declarations has much less structural reach. Afterwards, if the replacement works, shared classification infrastructure could be extracted for other pbfhogg consumers.

**Additional findings worth flagging.**

- **Malformed tag indices can silently shorten an element's tags.** Elivagar uses `tags()`, whose iterator stops at the first out-of-range string-table index. Block validation does not reject those indices globally. This can produce an archive from a partial tag list without reporting the corruption. [`TagIter::next`](/home/folk/Programs/elivagar/research/pbfhogg/src/read/elements.rs:748) implements that behavior; checked iterators already exist. A rewrite must consciously preserve it or declare rejection of corrupt input as a behavior correction.

- **The geocode scanner has a duplicate-string-table hazard outside Elivagar.** [`resolve_tag_indices`](/home/folk/Programs/elivagar/research/pbfhogg/src/scan/way.rs:385) records only the first ID for each literal, and subsequent matching compares IDs directly. A later dictionary entry containing identical text is therefore missed. The Shortbread classifier should classify every referenced ID, allowing multiple IDs to map to the same token.

- **pbfhogg's iteration APIs disagree on node order.** [`for_each_element`](/home/folk/Programs/elivagar/research/pbfhogg/src/read/block.rs:731) visits plain nodes before dense nodes within a group; [`elements()`](/home/folk/Programs/elivagar/research/pbfhogg/src/read/block.rs:1937) visits dense nodes first. Elivagar's node worker uses the former, and its sorted store requires strictly increasing IDs. A group mixing both representations can consequently behave differently between APIs. Switching to metadata-skipping iteration needs this discrepancy addressed.

- **Prepared-relation memory accounting omits match-owned allocations.** [`estimate_prepared_rel_bytes`](/home/folk/Programs/elivagar/crates/lib/src/pipeline/relations.rs:31) includes the inline struct and member geometry, but excludes attribute strings and spilled match/attribute vectors. Its reported inflight bytes can understate actual retained memory.
