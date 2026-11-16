-- The example benches: argv from params, the process ranked, the example's
-- own summary milliseconds recorded and nothing else.

local PMTILES_STDERR = "=== PMTiles Writer Benchmark ===\n"
    .. "  Generating tiles... done in 1.2s (500000 tiles, 300.0 MB compressed)\n\n"
    .. "  elivagar:   812 ms (310.2 MB)\n"
    .. "  pmtiles-rs: 1204 ms (311.0 MB)\n\n"
    .. "=== Summary (best of 1) ===\n"
    .. "  elivagar            812      310.2       615763\n"
    .. "  pmtiles-rs         1204      311.0       415282\n"
    .. "  -> elivagar is 1.48x faster\n"

local NODE_STDERR = "=== Node Store Benchmark ===\n"
    .. "  Building SortedNodeStore (run 1/1)... done in 900 ms\n"
    .. "  Way-like:  70000/70000 hits\n"
    .. "  Random:    10000000/10000000 hits\n\n"
    .. "=== Summary (best of 1) ===\n"
    .. "                     ms  lookups/sec  ns/lookup\n"
    .. "  build               900     55555556       18.0\n"
    .. "  way-like            120    583333333        1.7\n"
    .. "  random              340     29411765       34.0\n"

local BUILD_ONLY_STDERR = "=== Summary (build-only, best of 3) ===\n"
    .. "                     ms    nodes/sec    ns/node\n"
    .. "  build               880     56818182       17.6\n"

test("pmtiles-writer passes the typed argv and records both writers", function()
    local r = run { bench = "pmtiles-writer", params = { "--tiles", "9" },
        calls = { { stderr = PMTILES_STDERR } } }
    assert(r.outcome == "completed", r.reason)
    local c = r.calls[1]
    assert(c.argv[1] == "--out-dir", tostring(c.argv[1]))
    local dir = tostring(c.argv[2])
    assert(dir:find("/out/archives", 1, true), dir)
    assert(table.concat(c.argv, " ", 3) == "--tiles 9 --runs 1", table.concat(c.argv, " ", 3))
    assert(c.measured == true and c.hotpath == "pmtiles-writer")
    assert(r.record.elivagar_ms == 812 and r.record.pmtiles_rs_ms == 1204)
end)

test("pmtiles-writer without its summary fails", function()
    local r = run { bench = "pmtiles-writer", calls = { { stderr = "" } } }
    assert(r.outcome == "failed", r.outcome)
end)

test("node-store passes every size and records the summary ms, not the hits", function()
    local r = run { bench = "node-store", params = { "--nodes", "50", "--runs", "2" },
        calls = { { stderr = NODE_STDERR } } }
    assert(r.outcome == "completed", r.reason)
    assert(table.concat(r.calls[1].argv, " ") == "--nodes 50 --ways 4000 --random 10 --runs 2",
        table.concat(r.calls[1].argv, " "))
    assert(r.record.build_ms == 900 and r.record.way_like_ms == 120 and r.record.random_ms == 340)
    for key in pairs(r.record) do
        assert(not key:find("hit"), key)
    end
end)

test("node-store build-only looks nothing up", function()
    local r = run { bench = "node-store", params = { "--build-only", "yes", "--runs", "3" },
        calls = { { stderr = BUILD_ONLY_STDERR } } }
    assert(r.outcome == "completed", r.reason)
    assert(r.calls[1].argv[#r.calls[1].argv] == "--build-only")
    assert(r.record.build_ms == 880 and r.record.way_like_ms == nil)
end)

test("node-store refuses a node count past the store's ID cap at bind", function()
    local r = run { bench = "node-store", params = { "--nodes", "1001" } }
    assert(r.stage == "bind" and r.outcome == "refused", r.outcome)
end)

test("a zero run count is refused at bind", function()
    local r = run { bench = "node-store", params = { "--runs", "0" } }
    assert(r.stage == "bind" and r.outcome == "refused", r.outcome)
end)
