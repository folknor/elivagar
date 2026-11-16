-- pmtiles-writer: the crates/lib bench_pmtiles example, the workload the
-- typed `brokkr pmtiles-writer` measured. One process per iteration:
-- generate `tiles` synthetic gzipped tiles across z0-z14, then write them
-- `runs` times with elivagar's PmtilesWriter and `runs` times with
-- pmtiles-rs's writer. The result is that process's wall, generation and
-- both writers included, as typed --bench recorded it (there --bench N
-- became --runs N in one process; here --iterations repeats processes and
-- runs repeats the writers inside one, default 1).
--
-- The example reports each writer's best-of-runs milliseconds on stderr;
-- those are measurements and are recorded, never ranked on. Its closing
-- "faster" line compares the two writers and is not recorded.
--
-- The example writes bench_elivagar.pmtiles and bench_pmtiles_rs.pmtiles
-- into its required --out-dir, here a fresh output() under the
-- iteration's out/, so the writes are declared to brokkr and removed with
-- the iteration.
--
--   brokkr bench pmtiles-writer --tiles 2000000
--   brokkr bench --hotpath pmtiles-writer

function bench(params)
    local out = output("archives")
    local r = binary_with({ measured = true, hotpath = "pmtiles-writer" },
        "--out-dir", out, "--tiles", params.tiles, "--runs", params.runs):ok()
    if params.verbose == "yes" then print(r.stderr) end
    local eli = r.stderr:match("\n  elivagar:%s+(%d+) ms")
    local rs = r.stderr:match("\n  pmtiles%-rs:%s+(%d+) ms")
    if not eli or not rs then fail("bench_pmtiles did not report both writers' ms") end
    record.elivagar_ms = tonumber(eli)
    record.pmtiles_rs_ms = tonumber(rs)
end
