-- Span-end aliases shared by validation and child options: FOO, -FOO,
-- FOO_END and -FOO_END all resolve to FOO_END. Use =FOO for an exact
-- marker name, without appending _END (=PHASE12_START, say).
-- stop_at matches verbatim, with no runtime fallback, and brokkr owns the
-- required-stop postcondition: a child that exits or stops anywhere else
-- fails the iteration, so a renamed marker never turns a phase measurement
-- into a full run.
--
-- elivagar emits phase-boundary markers and nothing else (AGENTS.md,
-- Sidecar profiler), so a name outside that set is a typo and is refused
-- here, at validate, rather than as a failed run after the build.
local M = {}

M.MARKERS = {
    PHASE12_START = true, PHASE12_END = true,
    OCEAN_START = true, OCEAN_END = true,
    SORT_START = true, SORT_END = true,
    ASSEMBLE_START = true, ASSEMBLE_END = true,
}

local GRAMMAR = "--stop must be FOO_END, FOO, -FOO, -FOO_END or =MARKER (exact)"

function M.resolve(value)
    if value == nil then return nil end
    if not value:match("^%-?[%w_]+$") and not value:match("^=[%w_]+$") then
        fail(GRAMMAR)
    end
    local marker
    if value:sub(1, 1) == "=" then
        marker = value:sub(2)
    else
        marker = value:gsub("^-", "")
        if marker:sub(-4) ~= "_END" then
            marker = marker .. "_END"
        end
        if marker == "_END" then fail(GRAMMAR) end
    end
    if not M.MARKERS[marker] then
        fail("--stop " .. value .. " resolves to " .. marker .. ", which elivagar never emits; "
            .. "markers are PHASE12, OCEAN, SORT and ASSEMBLE, each _START and _END")
    end
    return marker
end

return M
