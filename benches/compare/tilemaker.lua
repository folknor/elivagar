-- tilemaker: Tilemaker with the shortbread-tilemaker config over a
-- dataset's PBF variant, the comparison the typed `brokkr tilemaker
-- --bench` measured, same flags (--fast, in-memory). Registered in
-- brokkr.d/benches.toml. The result is the tilemaker child's wall
-- (measured = true), launch to reap. The archive is never read: a bench
-- times, it does not compare the two producers' output.
--
-- The pinned tilemaker build, the pinned shortbread-tilemaker checkout, the
-- EPSG:4326 ocean and the config with absolute shapefile paths are
-- installed once by `brokkr run elivagar-tools -- tilemaker`
-- (crates/tools/src/tilemaker.rs, which holds the pins). The preflight
-- refuses when any is absent; each run records the two pinned commits from
-- the install markers.
--
-- --config is that resolved copy, not the checkout's config.json.
-- Tilemaker opens a layer's shapefile relative to its working directory,
-- which for a bench child is the project root, and silently skips one it
-- cannot open; typed tilemaker passed the checkout's config, so the
-- boundary_labels points were never read and the ocean only by the
-- accident of a `data` data dir. The installer refuses a config whose
-- shapefiles do not exist.
--
--   brokkr run elivagar-tools -- tilemaker
--   brokkr bench tilemaker --dataset denmark

local INSTALL = "brokkr run elivagar-tools -- tilemaker"

local function install()
    local data = config().host.data
    return {
        tilemaker = data .. "/tilemaker-build/tilemaker",
        config = data .. "/shortbread-tilemaker-config.json",
        process = data .. "/shortbread-tilemaker/process.lua",
        tilemaker_marker = data .. "/.tilemaker-version",
        shortbread_marker = data .. "/.shortbread-tilemaker-version",
    }
end

local function first_line(r)
    return r.stdout:match("^([^\n]*)")
end

function preflight()
    local i = install()
    local checks = {
        { "-x", i.tilemaker }, { "-f", i.config }, { "-f", i.process },
        { "-f", i.tilemaker_marker }, { "-f", i.shortbread_marker },
    }
    for _, check in ipairs(checks) do
        local r = binary_with({ program = "test" }, check[1], check[2])
        if r.start_error or r.exit ~= 0 then
            fail("tilemaker install incomplete: " .. check[2] .. " is missing\nFix: " .. INSTALL)
        end
    end
end

function bench(params)
    local i = install()
    record.tilemaker = first_line(binary_with({ program = "cat" }, i.tilemaker_marker):ok())
    record.shortbread_tilemaker = first_line(binary_with({ program = "cat" }, i.shortbread_marker):ok())

    local r = binary_with({ program = i.tilemaker, measured = true },
        "--input", params.variant.file,
        "--output", output("tilemaker.pmtiles"),
        "--config", i.config,
        "--process", i.process,
        "--fast"
    ):ok()
    if params.verbose == "yes" then
        print(r.stdout)
        print(r.stderr)
    end
end
