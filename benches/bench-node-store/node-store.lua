-- node-store: the crates/lib bench_node_store example, the workload the
-- typed `brokkr node-store` measured. One process per iteration: generate
-- `nodes` million synthetic nodes, build a SortedNodeStore once, then run
-- the way-like and random lookup loops `runs` times each; with build-only,
-- build the store `runs` times and look nothing up. The result is that
-- process's wall, as typed --bench recorded it (there --bench N became
-- --runs N in one process; here --iterations repeats processes, default
-- runs 1).
--
-- The example ends its stderr with a key=value vocabulary of integer
-- measurements, documented in its header; this script records exactly
-- those keys, shown and never ranked on. Its hit counts are answers about
-- the store, stay in the prose, and are not recorded.
--
--   brokkr bench node-store --nodes 100
--   brokkr bench node-store --build-only yes --runs 3
--   brokkr bench --alloc node-store

local BUILD_KEYS = { "nodes", "build_best_us", "convert_us" }
local LOOKUP_KEYS = {
    "way_lookups", "way_lookup_best_us",
    "random_lookups", "random_lookup_best_us",
}

local function take(stderr, keys)
    for _, key in ipairs(keys) do
        local v = stderr:match("\n" .. key .. "=(%d+)\n")
        if not v then fail("bench_node_store did not report " .. key .. "=") end
        record[key] = tonumber(v)
    end
end

function bench(params)
    local args = {
        "--nodes", params.nodes, "--ways", params.ways,
        "--random", params.random, "--runs", params.runs,
    }
    if params["build-only"] == "yes" then args[#args + 1] = "--build-only" end
    local r = binary_with({ measured = true, hotpath = "node-store" }, table.unpack(args)):ok()
    if params.verbose == "yes" then print(r.stderr) end
    take(r.stderr, BUILD_KEYS)
    if params["build-only"] ~= "yes" then take(r.stderr, LOOKUP_KEYS) end
end
