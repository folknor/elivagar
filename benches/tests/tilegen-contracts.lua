-- tilegen: the argv the bench builds from a host block against the in-tree
-- producer's spelling (crates/devkit tilegen.rs run_args), its resumes,
-- stops and refusals. Synthetic host and empty input only.
local DATA = project_root .. "/data"
local PBF = DATA .. "/dk.pbf"

local PRODUCTION = { ocean = {
    "z0-z7:simplified-water-polygons-split-3857/simplified_water_polygons.shp",
    "z8-z14:water-polygons-split-3857/water_polygons.shp",
    "ocean-tiles.pmtiles",
} }

local function host(block, entry)
    entry = entry or {}
    entry.file = "dk.pbf"
    return {
        tilegen = { default = block },
        datasets = { denmark = { pbf = { raw = entry } } },
    }
end

local function invoke(block, args, calls, entry)
    return run {
        bench = "tilegen", program = "elivagar", host = host(block, entry),
        params = args or {}, files = { [PBF] = "" }, calls = calls or { {} },
    }
end

local function words(call)
    return table.concat(call.argv, " ")
end

local function after(call, from)
    local rest = {}
    for i = from, #call.argv do rest[#rest + 1] = call.argv[i] end
    return table.concat(rest, " ")
end

test("the production block expands to the producer's ocean flags", function()
    local r = invoke(PRODUCTION)
    assert(r.outcome == "completed", r.reason)
    local c = r.calls[1]
    assert(c.argv[1] == "run" and c.argv[2] == PBF, words(c))
    assert(c.argv[3] == "-o" and c.origins[4] == "output", words(c))
    assert(c.argv[5] == "--tmp-dir" and c.origins[6] == "scratch", words(c))
    assert(after(c, 7) == "--ocean z0-z7:" .. DATA .. "/simplified-water-polygons-split-3857/simplified_water_polygons.shp"
        .. " --ocean z8-z14:" .. DATA .. "/water-polygons-split-3857/water_polygons.shp"
        .. " --ocean " .. DATA .. "/ocean-tiles.pmtiles", words(c))
    assert(c.measured == true and c.hotpath == "tilegen", words(c))
    assert(c.stop_at == nil, tostring(c.stop_at))
    assert(r.verdict == nil, tostring(r.verdict))
end)

-- BARE stands for the empty block: a fixture's {} could reach the schema as
-- an array, and in_memory = false emits nothing.
local BARE = { in_memory = false }

test("a block of defaults is a bare no-ocean run", function()
    local r = invoke(BARE)
    assert(r.outcome == "completed", r.reason)
    assert(#r.calls[1].argv == 6, words(r.calls[1]))
end)

test("every key lands in the producer's order and spelling", function()
    local r = invoke({
        compression_level = 9, allow_unsafe_flat_index = true, tile_format = "mvt",
        tile_compression = "gzip", compress_sort_chunks = "lz4", in_memory = true,
        threads = 8, sort_budget = "256M", way_budget = "1G", assemble_budget = "1048576",
        dedup_cap = 1000, fanout_cap_default = 4, fanout_caps = { water = 2, land = 3 },
        polygon_simplify_factor = 1.5, ocean = { "z0-z14:/abs/water.shp" },
    }, nil, nil, { locations_on_ways = true, force_sorted = true })
    assert(r.outcome == "completed", r.reason)
    assert(after(r.calls[1], 7) == "--compression-level 9 --force-sorted --locations-on-ways"
        .. " --allow-unsafe-flat-index --tile-format mvt --tile-compression gzip"
        .. " --compress-sort-chunks lz4 --in-memory -j 8 --sort-budget 268435456"
        .. " --way-budget 1073741824 --assemble-budget 1048576 --dedup-cap 1000"
        .. " --fanout-cap-default 4 --fanout-cap land=3,water=2"
        .. " --polygon-simplify-factor 1.5 --ocean z0-z14:/abs/water.shp", words(r.calls[1]))
end)

test("a resume makes its checkpoint in an untimed setup run", function()
    local r = invoke(PRODUCTION, { "--skip-to", "ocean" },
        { { stopped_at = "OCEAN_START" }, {} })
    assert(r.outcome == "completed", r.reason)
    local setup, measured = r.calls[1], r.calls[2]
    assert(setup.stop_at == "OCEAN_START" and not setup.measured and setup.hotpath == nil, words(setup))
    assert(not words(setup):find("--skip-to", 1, true), words(setup))
    assert(measured.measured == true and measured.argv[7] == "--skip-to" and measured.argv[8] == "ocean", words(measured))
    assert(setup.argv[6] == measured.argv[6], "the resume must read the setup's tmp dir")
    assert(after(setup, 7) == after(measured, 9), "setup and resume must share the block's flags")
end)

test("sort and assemble resumes stop their setup after the completion record", function()
    for _, phase in ipairs({ "sort", "assemble" }) do
        local r = invoke(BARE,{ "--skip-to", phase }, { { stopped_at = "SORT_END" }, {} })
        assert(r.outcome == "completed", r.reason)
        assert(r.calls[1].stop_at == "SORT_END", phase)
    end
end)

test("a span stop resolves to its end and returns stopped", function()
    local r = invoke(BARE,{ "--stop", "OCEAN" }, { { stopped_at = "OCEAN_END" } })
    assert(r.outcome == "completed", r.reason)
    assert(r.calls[1].stop_at == "OCEAN_END", tostring(r.calls[1].stop_at))
    assert(r.verdict == "stopped" and r.record.stopped_at == "OCEAN_END", tostring(r.verdict))
end)

test("a missed stop fails rather than timing a full run", function()
    local r = invoke(BARE,{ "--stop", "SORT" }, { { exit = 0 } })
    assert(r.outcome == "failed", r.outcome)
end)

test("a marker elivagar never emits is refused at validate", function()
    local r = invoke(BARE,{ "--stop", "PASS1" }, {})
    assert(r.stage == "validate" and r.outcome == "refused", r.outcome)
end)

test("a marker a resume skips is refused at validate", function()
    local r = invoke(BARE,{ "--skip-to", "sort", "--stop", "=OCEAN_START" }, {})
    assert(r.stage == "validate" and r.outcome == "refused", r.outcome)
    assert(string.find(r.reason, "never emits", 1, true), r.reason)
end)

-- Only blocks brokkr's own host schema loads: it already refuses unknown
-- keys, bad bands, out-of-range levels and budgets at config load, so a
-- fixture holding one never reaches the script. These two it accepts.
local REFUSED = {
    { "the retired seam_reconcile_layers", { seam_reconcile_layers = { boundaries = 8 } } },
    { "a simplify factor needing 17 digits", { polygon_simplify_factor = 0.1 + 0.2 } },
}

for _, case in ipairs(REFUSED) do
    test("a block with " .. case[1] .. " is refused before the build", function()
        local r = invoke(case[2], nil, {})
        assert(r.stage == "validate" and r.outcome == "refused", tostring(r.stage) .. " " .. r.outcome)
    end)
end
