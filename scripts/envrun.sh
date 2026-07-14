#!/bin/bash
# Run a command with env assignments, from inside the Claude harness.
#
# The harness permission matcher cannot express "allow brokkr with any
# number of env assignments set to any values", so `VAR=x brokkr ...` is
# blocked outright. This wrapper exists only because that rule cannot be
# written, not because the env prefix is something to gate.
#
# Preserves cwd (brokkr must run from the project root). Everything after
# the assignments is exec'd verbatim.
exec env "$@"
