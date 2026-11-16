-- The competitor benches (benches/compare): the argv each builds against
-- the installed layout crates/tools writes, the provenance they record,
-- and their refusals. Synthetic host and a small input only.
local DATA = project_root .. "/data"
local PBF = DATA .. "/dk.pbf"
local ITERATION = "/brokkr-contract/iteration"

local PRODUCTION = {
    "z0-z7:simplified-water-polygons-split-3857/simplified_water_polygons.shp",
    "z8-z14:water-polygons-split-3857/water_polygons.shp",
    "ocean-tiles.pmtiles",
}

local function host(ocean)
    return {
        tilegen = { default = { ocean = ocean } },
        datasets = { denmark = { pbf = { raw = { file = "dk.pbf" } } } },
    }
end

local function ok_calls(n)
    local calls = {}
    for i = 1, n do calls[i] = {} end
    return calls
end

local JDK_MARKER = "jdk-25.0.4.1+1\ndbb698396d478e7fa2b1e50f4103324b2a99b90569ee27c33f2261f9215cf41e\n"
local JAR_MARKER = "v0.10.2\nf310bd0413e2e4512b27f4046d418664e8e1d3bf31603c2a70e23de06c167e4d\n"

local function planetiler(spec)
    return run {
        bench = "planetiler",
        params = spec.params or {},
        host = host(spec.ocean or PRODUCTION),
        files = { [PBF] = "pbf" },
        -- java -version, then test -f on the jar, both markers and the ocean.
        preflight_calls = spec.preflight_calls or ok_calls(5),
        calls = spec.calls or {
            { stdout = JDK_MARKER }, { stdout = JAR_MARKER }, {},
        },
    }
end

local function starts(s, prefix)
    return type(s) == "string" and s:sub(1, #prefix) == prefix
end

local function contains(list, value)
    for _, v in ipairs(list or {}) do
        if v == value then return true end
    end
    return false
end

test("planetiler runs the pinned jar on the block's full-resolution ocean, no download", function()
    local r = planetiler {}
    assert(r.outcome == "completed", r.reason)
    local c = r.calls[3]
    local a = c.argv
    assert(c.program == DATA .. "/jdk/bin/java", tostring(c.program))
    assert(c.measured == true)
    assert(a[1] == "-Xmx2048m", a[1])
    assert(a[2] == "-jar" and a[3] == DATA .. "/planetiler.jar", a[3])
    assert(a[4] == "shortbread" and a[5] == "--force", table.concat(a, " "))
    assert(a[6] == "--osm_path=" .. PBF, a[6])
    assert(a[7] == "--ocean_path=" .. DATA .. "/water-polygons-split-3857/water_polygons.shp", a[7])
    assert(starts(a[8], "--output=" .. ITERATION) and a[8]:sub(-8) == ".pmtiles", a[8])
    assert(starts(a[9], "--tmpdir=" .. ITERATION), a[9])
    assert(a[10] == "--nodemap_type=sparsearray" and a[11] == "--maxzoom=14" and #a == 11,
        table.concat(a, " "))
    for _, word in ipairs(a) do
        assert(not word:find("download"), word)
    end
    assert(contains(c.reads, PBF), "the PBF is not declared read")
    assert(#c.writes == 2, tostring(#c.writes))
    assert(r.record.heap_mb == 2048, tostring(r.record.heap_mb))
    assert(r.record.jdk == "jdk-25.0.4.1+1", tostring(r.record.jdk))
    assert(r.record.planetiler == "v0.10.2", tostring(r.record.planetiler))
end)

test("planetiler reads the install markers before the measured child", function()
    local r = planetiler {}
    assert(r.outcome == "completed", r.reason)
    assert(r.calls[1].program == "cat" and r.calls[1].argv[1] == DATA .. "/.jdk-version")
    assert(r.calls[2].program == "cat" and r.calls[2].argv[1] == DATA .. "/.planetiler-version")
    assert(not r.calls[1].measured and not r.calls[2].measured)
end)

test("planetiler takes a single z0-z14 shapefile as its ocean", function()
    local r = planetiler { ocean = { "z0-z14:/abs/water_polygons.shp" } }
    assert(r.outcome == "completed", r.reason)
    assert(r.calls[3].argv[7] == "--ocean_path=/abs/water_polygons.shp", r.calls[3].argv[7])
end)

-- A block with no ocean is a valid no-ocean tilegen block (the host schema
-- refuses a malformed one before the script sees it), but planetiler has
-- no ocean to read from it.
test("planetiler refuses at validate a block with no full-resolution shapefile", function()
    local r = run {
        bench = "planetiler",
        host = {
            tilegen = { default = { in_memory = false } },
            datasets = { denmark = { pbf = { raw = { file = "dk.pbf" } } } },
        },
        files = { [PBF] = "pbf" },
    }
    assert(r.stage == "validate" and r.outcome == "refused", r.stage .. " " .. r.outcome)
    assert(r.reason:find("z8%-z14"), r.reason)
end)

test("planetiler without its JDK is refused at preflight with the install command", function()
    local r = planetiler {
        preflight_calls = { { start_error = { operation = "resolve", message = "No such file or directory" } } },
        calls = {},
    }
    assert(r.stage == "preflight" and r.outcome == "refused", r.stage .. " " .. r.outcome)
    assert(r.reason:find("elivagar-tools -- planetiler", 1, true), r.reason)
    assert(r.preflight_calls[1].program == DATA .. "/jdk/bin/java", tostring(r.preflight_calls[1].program))
end)

test("planetiler without the ocean shapefile is refused at preflight", function()
    local calls = ok_calls(4)
    calls[5] = { exit = 1 }
    local r = planetiler { preflight_calls = calls, calls = {} }
    assert(r.stage == "preflight" and r.outcome == "refused", r.stage .. " " .. r.outcome)
    assert(r.reason:find("download-ocean", 1, true), r.reason)
end)

test("a failed planetiler child fails the run", function()
    local r = planetiler { calls = { { stdout = JDK_MARKER }, { stdout = JAR_MARKER }, { exit = 1 } } }
    assert(r.outcome == "failed", r.outcome)
end)

local function tilemaker(spec)
    return run {
        bench = "tilemaker",
        params = spec.params or {},
        host = host(PRODUCTION),
        files = { [PBF] = "pbf" },
        -- test on the binary, the config, process.lua and both markers.
        preflight_calls = spec.preflight_calls or ok_calls(5),
        calls = spec.calls or {
            { stdout = "facdf933f4f5ac2944a9d65c27970270583d68fa" },
            { stdout = "9ec009826b49b71bb1481a1c28c3681766c600e3" },
            {},
        },
    }
end

test("tilemaker runs the pinned build with the resolved config", function()
    local r = tilemaker {}
    assert(r.outcome == "completed", r.reason)
    local c = r.calls[3]
    local a = c.argv
    assert(c.program == DATA .. "/tilemaker-build/tilemaker" and c.measured == true, tostring(c.program))
    assert(a[1] == "--input" and a[2] == PBF and c.origins[2] == "input", table.concat(a, " "))
    assert(a[3] == "--output" and c.origins[4] == "output" and a[4]:sub(-8) == ".pmtiles", a[4])
    assert(a[5] == "--config" and a[6] == DATA .. "/shortbread-tilemaker-config.json", a[6])
    assert(a[7] == "--process" and a[8] == DATA .. "/shortbread-tilemaker/process.lua", a[8])
    assert(a[9] == "--fast" and #a == 9, table.concat(a, " "))
    assert(r.record.tilemaker == "facdf933f4f5ac2944a9d65c27970270583d68fa", tostring(r.record.tilemaker))
    assert(r.record.shortbread_tilemaker == "9ec009826b49b71bb1481a1c28c3681766c600e3")
end)

test("tilemaker never passes the checkout's own config", function()
    local r = tilemaker {}
    assert(r.outcome == "completed", r.reason)
    for _, word in ipairs(r.calls[3].argv) do
        assert(not word:find("shortbread%-tilemaker/config%.json"), word)
    end
end)

test("tilemaker without its resolved config is refused at preflight", function()
    -- The binary passes; the config check is the last call made.
    local r = tilemaker { preflight_calls = { {}, { exit = 1 } }, calls = {} }
    assert(r.stage == "preflight" and r.outcome == "refused", r.stage .. " " .. r.outcome)
    assert(r.reason:find("shortbread-tilemaker-config.json", 1, true), r.reason)
    assert(r.reason:find("elivagar-tools -- tilemaker", 1, true), r.reason)
end)
