-- planetiler: Planetiler's bundled Shortbread profile over a dataset's PBF
-- variant, the comparison the typed `brokkr planetiler --bench` measured.
-- Registered in brokkr.d/benches.toml. The result is the java child's wall
-- (measured = true), launch to reap, JVM startup included, as typed --bench
-- reported. The archive is never read: a bench times, it does not compare
-- the two producers' output.
--
-- The runtime is the pinned Temurin JDK and Planetiler release jar under the
-- host's data dir, installed once by `brokkr run elivagar-tools --
-- planetiler` (crates/tools/src/planetiler.rs, which holds the pins). The
-- preflight refuses when it is absent; each run records the release lines
-- of the two install markers. Nothing is compiled, which is what typed
-- planetiler got wrong on elivagar: it compiled a Java baseline class this
-- tree never had, javac failed every run, and no row was ever stored.
--
-- No network inside the measured wall, and none at all: --download is never
-- passed, so planetiler fetches nothing, and the ocean source is given as
-- --ocean_path, the full-resolution EPSG:3857 shapefile the host's tilegen
-- block hands elivagar (its z8-z14 or z0-z14 entry). Both producers read the
-- same ocean release. Planetiler's shortbread schema reads that and the OSM
-- file only; it has no natural-earth source. Typed planetiler primed a
-- `data/sources` download instead, a different, unpinned ocean release, via
-- a `--download_dir` flag planetiler does not have (its custom-map driver
-- hard-codes `data/sources` under the working directory).
--
-- Heap: max(2 x the PBF's decimal megabytes, 2048) MiB, typed planetiler's
-- policy, recorded as heap_mb. Output and --tmpdir are iteration paths.
--
--   brokkr run elivagar-tools -- planetiler
--   brokkr bench planetiler --dataset denmark
--   brokkr bench --iterations 1 planetiler --dataset germany --variant raw

local tilegen = require("tilegen")

local MIN_HEAP_MB = 2048
local HEAP_PER_FILE_MB = 2
local INSTALL = "brokkr run elivagar-tools -- planetiler"

local function runtime()
    local data = config().host.data
    return {
        java = data .. "/jdk/bin/java",
        jar = data .. "/planetiler.jar",
        jdk_marker = data .. "/.jdk-version",
        jar_marker = data .. "/.planetiler-version",
    }
end

-- The full-resolution shapefile among the block's ocean flags, already
-- joined onto the data dir the way elivagar is given it.
local function ocean_shapefile(params, keys)
    local flags = tilegen.flags(keys.tilegen, params.tilegen, params.variant, config().host.data)
    for i = 1, #flags - 1 do
        if flags[i] == "--ocean" then
            local path = flags[i + 1]:match("^z8%-z14:(.+)$") or flags[i + 1]:match("^z0%-z14:(.+)$")
            if path then return path end
        end
    end
    fail("[<host>.tilegen." .. keys.tilegen .. "].ocean names no z8-z14 or z0-z14 shapefile, "
        .. "which planetiler is given as its ocean source")
end

local function int_text(v)
    return (tostring(v):gsub("%.0$", ""))
end

local function first_line(r)
    return r.stdout:match("^([^\n]*)")
end

function validate(params, keys)
    ocean_shapefile(params, keys)
end

function preflight(params, keys)
    local rt = runtime()
    local java = binary_with({ program = rt.java }, "-version")
    if java.start_error or java.exit ~= 0 then
        fail("no runnable JDK at " .. rt.java .. "\nFix: " .. INSTALL)
    end
    local checks = {
        { rt.jar, "no Planetiler jar at " .. rt.jar .. "\nFix: " .. INSTALL },
        { rt.jdk_marker, "no JDK install marker at " .. rt.jdk_marker .. "\nFix: " .. INSTALL },
        { rt.jar_marker, "no Planetiler install marker at " .. rt.jar_marker .. "\nFix: " .. INSTALL },
        { ocean_shapefile(params, keys), "the ocean shapefile is missing\n"
            .. "Fix: brokkr run elivagar-datasets -- download-ocean" },
    }
    for _, check in ipairs(checks) do
        local r = binary_with({ program = "test" }, "-f", check[1])
        if r.start_error or r.exit ~= 0 then fail(check[2] .. " (" .. check[1] .. ")") end
    end
end

function bench(params, keys)
    local rt = runtime()
    local pbf = params.variant.file
    local ocean = ocean_shapefile(params, keys)
    local out = output("planetiler.pmtiles")
    local tmp = scratch("planetiler-tmp")

    record.jdk = first_line(binary_with({ program = "cat" }, rt.jdk_marker):ok())
    record.planetiler = first_line(binary_with({ program = "cat" }, rt.jar_marker):ok())

    local heap = math.floor(filesize(pbf) / 1000000) * HEAP_PER_FILE_MB
    if heap < MIN_HEAP_MB then heap = MIN_HEAP_MB end
    record.heap_mb = heap

    local r = binary_with({
        program = rt.java,
        measured = true,
        reads = { pbf },
        writes = { out, tmp },
    },
        "-Xmx" .. int_text(heap) .. "m",
        "-jar", rt.jar,
        "shortbread",
        "--force",
        "--osm_path=" .. pbf.path,
        "--ocean_path=" .. ocean,
        "--output=" .. out.path,
        "--tmpdir=" .. tmp.path,
        "--nodemap_type=sparsearray",
        "--maxzoom=14"
    ):ok()
    if params.verbose == "yes" then
        print(r.stdout)
        print(r.stderr)
    end
end
