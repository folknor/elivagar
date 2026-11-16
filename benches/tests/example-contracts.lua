-- The example benches: argv from params, the process ranked, exactly the
-- example's key=value measurement vocabulary recorded and nothing else.

local PMTILES_STDERR = "=== PMTiles Writer Benchmark ===\n"
    .. "  Generating tiles... done in 1.2s (500000 tiles, 300.0 MB compressed)\n\n"
    .. "  elivagar:   812 ms (310.2 MB)\n"
    .. "  pmtiles-rs: 1204 ms (311.0 MB)\n\n"
    .. "=== Summary (best of 1) ===\n"
    .. "  elivagar            812      310.2       615763\n"
    .. "  pmtiles-rs         1204      311.0       415282\n"
    .. "  -> elivagar is 1.48x faster\n\n"
    .. "tiles=500000\n"
    .. "tile_bytes=300000000\n"
    .. "gen_us=1200345\n"
    .. "elivagar_best_us=812456\n"
    .. "elivagar_bytes=310200000\n"
    .. "pmtiles_rs_best_us=1204789\n"
    .. "pmtiles_rs_bytes=311000000\n"

local NODE_STDERR = "=== Node Store Benchmark ===\n"
    .. "  Building SortedNodeStore (run 1/1)... done in 900 ms\n"
    .. "  Converting to reader... done in 15 ms\n"
    .. "  Way-like:  70000/70000 hits\n"
    .. "  Random:    10000000/10000000 hits\n\n"
    .. "=== Summary (best of 1) ===\n"
    .. "                     ms  lookups/sec  ns/lookup\n"
    .. "  build               900     55555556       18.0\n"
    .. "  way-like            120    583333333        1.7\n"
    .. "  random              340     29411765       34.0\n\n"
    .. "nodes=50000000\n"
    .. "build_best_us=900123\n"
    .. "convert_us=15004\n"
    .. "way_lookups=70000\n"
    .. "way_lookup_best_us=120456\n"
    .. "random_lookups=10000000\n"
    .. "random_lookup_best_us=340789\n"

local BUILD_ONLY_STDERR = "=== Summary (build-only, best of 3) ===\n"
    .. "                     ms    nodes/sec    ns/node\n"
    .. "  build               880     56818182       17.6\n\n"
    .. "nodes=50000000\n"
    .. "build_best_us=880001\n"
    .. "convert_us=14002\n"

local function keys_of(t)
    local ks = {}
    for k in pairs(t) do ks[#ks + 1] = k end
    table.sort(ks)
    return table.concat(ks, ",")
end

test("pmtiles-writer passes the typed argv and records exactly the vocabulary", function()
    local r = run { bench = "pmtiles-writer", params = { "--tiles", "9" },
        calls = { { stderr = PMTILES_STDERR } } }
    assert(r.outcome == "completed", r.reason)
    local c = r.calls[1]
    assert(c.argv[1] == "--out-dir", tostring(c.argv[1]))
    local dir = tostring(c.argv[2])
    assert(dir:find("/out/archives", 1, true), dir)
    assert(table.concat(c.argv, " ", 3) == "--tiles 9 --runs 1", table.concat(c.argv, " ", 3))
    assert(c.measured == true and c.hotpath == "pmtiles-writer")
    local k = keys_of(r.record)
    assert(k == "elivagar_best_us,elivagar_bytes,gen_us,pmtiles_rs_best_us,pmtiles_rs_bytes,tile_bytes,tiles", k)
    assert(r.record.elivagar_best_us == 812456 and r.record.pmtiles_rs_best_us == 1204789)
    assert(r.record.tiles == 500000 and r.record.elivagar_bytes == 310200000)
end)

test("pmtiles-writer without its vocabulary fails", function()
    local r = run { bench = "pmtiles-writer", calls = { { stderr = "" } } }
    assert(r.outcome == "failed", r.outcome)
end)

test("pmtiles-writer does not scrape the prose summary", function()
    local prose = PMTILES_STDERR:gsub("\npmtiles_rs_bytes=%d+\n", "\n")
    local r = run { bench = "pmtiles-writer", calls = { { stderr = prose } } }
    assert(r.outcome == "failed", r.outcome)
end)

test("node-store passes every size and records the vocabulary, not the hits", function()
    local r = run { bench = "node-store", params = { "--nodes", "50", "--runs", "2" },
        calls = { { stderr = NODE_STDERR } } }
    assert(r.outcome == "completed", r.reason)
    assert(table.concat(r.calls[1].argv, " ") == "--nodes 50 --ways 4000 --random 10 --runs 2",
        table.concat(r.calls[1].argv, " "))
    local k = keys_of(r.record)
    assert(k == "build_best_us,convert_us,nodes,random_lookup_best_us,random_lookups,way_lookup_best_us,way_lookups", k)
    assert(r.record.build_best_us == 900123 and r.record.way_lookup_best_us == 120456
        and r.record.random_lookup_best_us == 340789)
    for key in pairs(r.record) do
        assert(not key:find("hit"), key)
    end
end)

test("node-store without its lookup keys fails", function()
    local r = run { bench = "node-store", calls = { { stderr = BUILD_ONLY_STDERR } } }
    assert(r.outcome == "failed", r.outcome)
end)

test("node-store build-only looks nothing up", function()
    local r = run { bench = "node-store", params = { "--build-only", "yes", "--runs", "3" },
        calls = { { stderr = BUILD_ONLY_STDERR } } }
    assert(r.outcome == "completed", r.reason)
    assert(r.calls[1].argv[#r.calls[1].argv] == "--build-only")
    assert(keys_of(r.record) == "build_best_us,convert_us,nodes", keys_of(r.record))
    assert(r.record.build_best_us == 880001)
end)

test("node-store refuses a node count past the store's ID cap at bind", function()
    local r = run { bench = "node-store", params = { "--nodes", "1001" } }
    assert(r.stage == "bind" and r.outcome == "refused", r.outcome)
end)

test("a zero run count is refused at bind", function()
    local r = run { bench = "node-store", params = { "--runs", "0" } }
    assert(r.stage == "bind" and r.outcome == "refused", r.outcome)
end)
