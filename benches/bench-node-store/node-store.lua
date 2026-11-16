-- node-store: the crates/lib bench_node_store example, the workload the
-- typed `brokkr node-store` measured. One process per iteration: generate
-- `nodes` million synthetic nodes, build a SortedNodeStore once, then run
-- the way-like and random lookup loops `runs` times each; with build-only,
-- build the store `runs` times and look nothing up. The result is that
-- process's wall, as typed --bench recorded it (there --bench N became
-- --runs N in one process; here --iterations repeats processes, default
-- runs 1).
--
-- The example's summary milliseconds are recorded as measurements, never
-- ranked on. Its hit counts are answers about the store and are not.
--
--   brokkr bench node-store --nodes 100
--   brokkr bench node-store --build-only yes --runs 3
--   brokkr bench --alloc node-store

function bench(params)
    local args = {
        "--nodes", params.nodes, "--ways", params.ways,
        "--random", params.random, "--runs", params.runs,
    }
    if params["build-only"] == "yes" then args[#args + 1] = "--build-only" end
    local r = binary_with({ measured = true, hotpath = "node-store" }, table.unpack(args)):ok()
    if params.verbose == "yes" then print(r.stderr) end
    local build = r.stderr:match("\n  build%s+(%d+) ")
    if not build then fail("bench_node_store did not report its build ms") end
    record.build_ms = tonumber(build)
    if params["build-only"] ~= "yes" then
        local way = r.stderr:match("\n  way%-like%s+(%d+) ")
        local random = r.stderr:match("\n  random%s+(%d+) ")
        if not way or not random then fail("bench_node_store did not report its lookup ms") end
        record.way_like_ms = tonumber(way)
        record.random_ms = tonumber(random)
    end
end
