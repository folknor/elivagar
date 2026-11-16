-- pmtiles-writer: the crates/lib bench_pmtiles example, the workload the
-- typed `brokkr pmtiles-writer` measured. One process per iteration:
-- generate `tiles` synthetic gzipped tiles across z0-z14, then write them
-- `runs` times with elivagar's PmtilesWriter and `runs` times with
-- pmtiles-rs's writer. The result is that process's wall, generation and
-- both writers included, as typed --bench recorded it (there --bench N
-- became --runs N in one process; here --iterations repeats processes and
-- runs repeats the writers inside one, default 1).
--
-- The example ends its stderr with a key=value vocabulary of integer
-- measurements, documented in its header; this script records exactly
-- those keys, shown and never ranked on. Its prose "faster" line compares
-- the two writers and is not recorded.
--
-- The example writes bench_elivagar.pmtiles and bench_pmtiles_rs.pmtiles
-- into its required --out-dir, here a fresh output() under the
-- iteration's out/, so the writes are declared to brokkr and removed with
-- the iteration.
--
--   brokkr bench pmtiles-writer --tiles 2000000
--   brokkr bench --hotpath pmtiles-writer

local KEYS = {
    "tiles", "tile_bytes", "gen_us",
    "elivagar_best_us", "elivagar_bytes",
    "pmtiles_rs_best_us", "pmtiles_rs_bytes",
}

function bench(params)
    local out = output("archives")
    local r = binary_with({ measured = true, hotpath = "pmtiles-writer" },
        "--out-dir", out, "--tiles", params.tiles, "--runs", params.runs):ok()
    if params.verbose == "yes" then print(r.stderr) end
    for _, key in ipairs(KEYS) do
        local v = r.stderr:match("\n" .. key .. "=(%d+)\n")
        if not v then fail("bench_pmtiles did not report " .. key .. "=") end
        record[key] = tonumber(v)
    end
end
