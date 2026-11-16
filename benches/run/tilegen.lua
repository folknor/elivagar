-- tilegen: one `elivagar run` of a dataset's PBF variant under the host's
-- tilegen block, the workload the typed `brokkr tilegen --bench` measured.
-- The block is bound as the `tilegen` param (host.tilegen.NAME), so the
-- whole contract is stored with the run, and expanded by lib/tilegen.lua
-- into exactly the flags the in-tree producer (crates/devkit, `brokkr run`)
-- passes, in its order. The bench's own paths differ, and only those: the
-- archive goes to an iteration output and --tmp-dir to an iteration
-- scratch directory, always given explicitly, so no run shares
-- data/tilegen_tmp with another elivagar process and nothing outlives the
-- iteration. The archive is never read: checking output is the corpus
-- gate's, `brokkr pmtiles-corpus check`.
--
-- The result is the elivagar child's wall (measured = true), launch to
-- reap, which is what typed --bench reported.
--
-- skip-to resumes from a checkpoint, as typed --skip-to did. A script bench
-- owns no state between iterations, so each iteration first makes that
-- checkpoint itself: an untimed setup run of the same argv without
-- --skip-to, stopped at the first marker after the checkpoint is on disk,
-- then the measured resumed run in the same tmp dir. That is worth its cost
-- because the resumed phases are otherwise unmeasurable in isolation:
-- typed --skip-to reused chunks whatever run last left them, from whichever
-- commit and block, with nothing in the recorded invocation saying which;
-- here the checkpoint is the same commit, block and input every time. The
-- setup costs wall-clock time per iteration (up to the end of the ocean
-- phase), never measurement. The stop points follow the pipeline's save
-- order: the PBF checkpoint is written after PHASE12_END and before
-- OCEAN_START, the chunk completion record after SORT_START.
--
-- stop ends the measured child at a phase marker. Under skip-to, markers of
-- skipped phases never fire and are refused at validate. Profiling marks
-- the measured child only; the setup child is not profiled. elivagar takes
-- the stop's SIGTERM on a signal thread that writes the hotpath report and
-- exits 143, so a stop under --hotpath or --alloc captures a report covering
-- the run up to the marker (crates/cli/src/signals.rs).
--
--   brokkr bench tilegen --dataset denmark --variant locations
--   brokkr bench tilegen --dataset denmark --skip-to assemble
--   brokkr bench --hotpath tilegen --dataset denmark
--   brokkr bench tilegen --dataset germany --stop PHASE12

local stops = require("stop")
local tilegen = require("tilegen")

-- Where each resume's setup run is stopped: the first marker emitted after
-- everything that resume reads is saved.
local SETUP_STOP = { ocean = "OCEAN_START", sort = "SORT_END", assemble = "SORT_END" }

-- Markers a resumed run never emits.
local SKIPPED = {
    ocean = { PHASE12_END = true },
    sort = { PHASE12_END = true, OCEAN_START = true },
    assemble = { PHASE12_END = true, OCEAN_START = true },
}

local function flags(params, keys)
    return tilegen.flags(keys.tilegen, params.tilegen, params.variant, config().host.data)
end

function validate(params, keys)
    local marker = stops.resolve(params.stop)
    local skip = params["skip-to"]
    if marker and skip and SKIPPED[skip][marker] then
        fail("--stop " .. params.stop .. " resolves to " .. marker
            .. ", which a --skip-to " .. skip .. " run never emits")
    end
    flags(params, keys)
end

function bench(params, keys)
    local configured = flags(params, keys)
    local input = params.variant.file
    local out = output("tiles.pmtiles")
    local tmp = scratch("tilegen_tmp")
    local skip = params["skip-to"]

    local function argv(resume, archive)
        local a = { "run", input, "-o", archive, "--tmp-dir", tmp }
        if resume then
            a[#a + 1] = "--skip-to"
            a[#a + 1] = resume
        end
        for _, f in ipairs(configured) do a[#a + 1] = f end
        return a
    end

    if skip then
        binary_with({ stop_at = SETUP_STOP[skip] }, table.unpack(argv(nil, output("setup.pmtiles")))):ok()
    end

    local r = binary_with({
        measured = true,
        hotpath = "tilegen",
        stop_at = stops.resolve(params.stop),
    }, table.unpack(argv(skip, out))):ok()

    if params.verbose == "yes" then
        print(r.stdout)
        print(r.stderr)
    end
    if r.stopped_at then
        record.stopped_at = r.stopped_at
        return "stopped"
    end
end
