-- The tilegen contract, `[<host>.tilegen.<name>]`, expanded to the flags
-- `elivagar run` is given. The reference spelling is crates/devkit's
-- tilegen.rs (`run_args` and `push_configured`): the same block and PBF
-- entry yield the same flags, in the same order, so a bench child's argv
-- reads like the plain `brokkr run` producer's apart from the input,
-- output and --tmp-dir paths, which are the bench's.
--
-- Every key devkit's deserializer refuses is refused here too, so a block
-- the producer would not run is not measured either. Values are passed as
-- the producer spells them: budgets as raw bytes, fanout caps as one
-- `layer=cap,...` list in key order, ocean paths joined onto the host's
-- data directory.
local M = {}

local KEYS = {
    ocean = true, compression_level = true, tile_format = true,
    tile_compression = true, compress_sort_chunks = true, in_memory = true,
    threads = true, sort_budget = true, way_budget = true,
    assemble_budget = true, dedup_cap = true, fanout_cap_default = true,
    fanout_caps = true, polygon_simplify_factor = true,
    allow_unsafe_flat_index = true,
}

local RETIRED = {
    seam_reconcile_layers = "elivagar removed --seam-reconcile-layers at 099611b, so no "
        .. "revision this bench builds accepts it; delete the key from the block",
}

local PBF_KEYS = {
    file = true, xxhash = true, seq = true, locations_on_ways = true,
    force_sorted = true, expect = true,
}

local MIB = 1024 * 1024
local EXACT = 9007199254740992 -- 2^53, the last integer a script number holds exactly
local U32_MAX = 4294967295

local function where(name, key)
    return "[<host>.tilegen." .. name .. "]." .. key
end

local function integral(v)
    return type(v) == "number" and v == v and v % 1 == 0
end

-- An unsigned integer within max; TOML integers beyond 2^53 arrive as
-- their decimal text, and are kept as that text.
local function unsigned(name, key, v, max)
    if type(v) == "string" and v:match("^%d+$") and max == nil then return v end
    if not integral(v) or v < 0 or (max ~= nil and v > max) then
        fail(where(name, key) .. " must be an unsigned integer"
            .. (max and (" up to " .. tostring(max)) or ""))
    end
    return v
end

-- A whole number as argv text, never "5.0" whatever the VM's number type.
local function int_text(v)
    if type(v) == "string" then return v end
    local s = tostring(v)
    return (s:gsub("%.0$", ""))
end

local function boolean(name, key, v)
    if v == nil then return false end
    if type(v) ~= "boolean" then fail(where(name, key) .. " must be true or false") end
    return v
end

local function choice(name, key, v, choices)
    if v == nil then return nil end
    for _, c in ipairs(choices) do
        if v == c then return v end
    end
    fail(where(name, key) .. " must be one of " .. table.concat(choices, ", ")
        .. ", got " .. tostring(v))
end

-- devkit's ByteSize: trimmed, an optional M/m or G/g suffix, then the
-- digits Rust's usize parser takes (a leading + included).
local function budget(name, key, v, minimum)
    if v == nil then return nil end
    if type(v) ~= "string" then
        fail(where(name, key) .. " must be a string: bytes, MiB (M) or GiB (G)")
    end
    local text = v:match("^%s*(.-)%s*$")
    local multiplier = 1
    local digits = text
    local last = text:sub(-1)
    if last == "M" or last == "m" then
        multiplier, digits = MIB, text:sub(1, -2)
    elseif last == "G" or last == "g" then
        multiplier, digits = MIB * 1024, text:sub(1, -2)
    end
    digits = digits:match("^%s*(.-)%s*$")
    if not digits:match("^%+?%d+$") then
        fail(where(name, key) .. ": expected bytes, MiB (M), or GiB (G), got " .. v)
    end
    local bytes = tonumber((digits:gsub("^%+", ""))) * multiplier
    if bytes > EXACT then
        fail(where(name, key) .. ": " .. v .. " is past 2^53 bytes, which this bench cannot spell exactly")
    end
    if bytes < minimum then
        fail(where(name, key) .. " must be at least " .. int_text(minimum) .. " bytes")
    end
    return bytes
end

-- One ocean entry in elivagar's --ocean spec, its path joined onto data.
local function ocean_spec(name, raw, data)
    if type(raw) ~= "string" then fail(where(name, "ocean") .. " entries must be strings") end
    local colon = raw:find(":", 1, true)
    local band, file
    if colon then
        band, file = raw:sub(1, colon - 1), raw:sub(colon + 1)
        if band ~= "z0-z14" and band ~= "z0-z7" and band ~= "z8-z14" then
            fail("unknown zoom band '" .. band .. "': elivagar implements one split, at z7/z8, "
                .. "so the only accepted bands are z0-z14, z0-z7 and z8-z14")
        end
    elseif raw:sub(-8) == ".pmtiles" then
        file = raw
    elseif raw:sub(-4) == ".shp" then
        fail("shapefile '" .. raw .. "' needs a zoom band prefix (z0-z14:, z0-z7: or z8-z14:)")
    else
        fail("'" .. raw .. "' is neither a zoom-banded shapefile nor a .pmtiles artifact")
    end
    if not file:match("%S") then fail("ocean input path is empty") end
    local path = file
    if file:sub(1, 1) ~= "/" then path = data .. "/" .. file end
    if band then return band .. ":" .. path end
    return path
end

local function sorted_keys(t)
    local keys = {}
    for k in pairs(t) do
        local i = #keys + 1
        while i > 1 and keys[i - 1] > k do
            keys[i] = keys[i - 1]
            i = i - 1
        end
        keys[i] = k
    end
    return keys
end

-- The flags for block `block` (named `name`) and PBF entry `entry`, after
-- `run INPUT -o OUTPUT --tmp-dir DIR [--skip-to PHASE]`. `data` is the
-- host's absolute data directory.
function M.flags(name, block, entry, data)
    if type(block) ~= "table" then fail("[<host>.tilegen." .. name .. "] must be a table") end
    for key in pairs(block) do
        if RETIRED[key] then
            fail(where(name, key) .. " is retired: " .. RETIRED[key])
        end
        if not KEYS[key] then fail(where(name, key) .. " is not a tilegen key") end
    end
    for key in pairs(entry) do
        if not PBF_KEYS[key] then fail("the PBF variant's " .. key .. " is not a pbf entry key") end
    end

    local args = {}
    local function push(...)
        for _, a in ipairs({ ... }) do args[#args + 1] = a end
    end

    if block.compression_level ~= nil then
        push("--compression-level", int_text(unsigned(name, "compression_level", block.compression_level, 10)))
    end
    for _, key in ipairs({ "force_sorted", "locations_on_ways" }) do
        if entry[key] ~= nil and type(entry[key]) ~= "boolean" then
            fail("the PBF variant's " .. key .. " must be true or false")
        end
    end
    if entry.force_sorted then push("--force-sorted") end
    if entry.locations_on_ways then push("--locations-on-ways") end
    if boolean(name, "allow_unsafe_flat_index", block.allow_unsafe_flat_index) then
        push("--allow-unsafe-flat-index")
    end
    local fmt = choice(name, "tile_format", block.tile_format, { "mvt", "mlt" })
    if fmt then push("--tile-format", fmt) end
    local comp = choice(name, "tile_compression", block.tile_compression, { "gzip", "brotli" })
    if comp then push("--tile-compression", comp) end
    local chunks = choice(name, "compress_sort_chunks", block.compress_sort_chunks, { "lz4", "snappy" })
    if chunks then push("--compress-sort-chunks", chunks) end
    if boolean(name, "in_memory", block.in_memory) then push("--in-memory") end
    if block.threads ~= nil then
        push("-j", int_text(unsigned(name, "threads", block.threads, U32_MAX)))
    end
    local sort = budget(name, "sort_budget", block.sort_budget, 64 * MIB)
    if sort then push("--sort-budget", int_text(sort)) end
    local way = budget(name, "way_budget", block.way_budget, MIB)
    if way then push("--way-budget", int_text(way)) end
    local assemble = budget(name, "assemble_budget", block.assemble_budget, MIB)
    if assemble then push("--assemble-budget", int_text(assemble)) end
    if block.dedup_cap ~= nil then
        push("--dedup-cap", int_text(unsigned(name, "dedup_cap", block.dedup_cap, nil)))
    end
    if block.fanout_cap_default ~= nil then
        push("--fanout-cap-default", int_text(unsigned(name, "fanout_cap_default", block.fanout_cap_default, U32_MAX)))
    end
    if block.fanout_caps ~= nil then
        if type(block.fanout_caps) ~= "table" then fail(where(name, "fanout_caps") .. " must be a table") end
        local specs = {}
        for _, layer in ipairs(sorted_keys(block.fanout_caps)) do
            local cap = unsigned(name, "fanout_caps." .. layer, block.fanout_caps[layer], U32_MAX)
            specs[#specs + 1] = layer .. "=" .. int_text(cap)
        end
        if #specs > 0 then push("--fanout-cap", table.concat(specs, ",")) end
    end
    local factor = block.polygon_simplify_factor
    if factor ~= nil then
        -- Rust's f64 Display is the shortest round-trip spelling and never an
        -- exponent; Lua's %.14g agrees with it exactly when the value
        -- round-trips at 14 digits and needs no exponent. Anything else is
        -- refused rather than passed in a spelling the producer would not use.
        local text = type(factor) == "number" and tostring(factor) or nil
        if text == nil or text:find("[eEni]") or tonumber(text) ~= factor then
            fail(where(name, "polygon_simplify_factor") .. " must be a number of at most 14 "
                .. "significant digits that needs no exponent")
        end
        push("--polygon-simplify-factor", (text:gsub("%.0$", "")))
    end
    local ocean = block.ocean
    if ocean ~= nil then
        if type(ocean) ~= "table" then fail(where(name, "ocean") .. " must be a list") end
        for _, raw in ipairs(ocean) do
            push("--ocean", ocean_spec(name, raw, data))
        end
    end
    return args
end

return M
