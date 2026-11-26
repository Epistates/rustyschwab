#!/usr/bin/env bash
# The cargo target runner for the mutation job (scheduled.yml). It runs one
# test binary under the memory cap. When the binary dies by a signal, it
# writes a marker naming the mutant: the change cargo-mutants made to the
# tree in place. An allocation failure under the cap aborts the binary, and
# the kernel's OOM killer sends SIGKILL, so either way no test's assertion
# stopped the mutant. The report job fails on any marker.
#
# Environment: MUTANT_TEST_MEMORY, the data limit in bytes, and
# MUTANT_MARKERS, the directory for markers.
set -u

prlimit --data="${MUTANT_TEST_MEMORY:?}" -- "$@"
status=$?

if [ "$status" -gt 128 ]; then
  mkdir -p "${MUTANT_MARKERS:?}"
  {
    echo "test binary: $1"
    echo "signal: $((status - 128))"
    echo "mutant:"
    git -C "${GITHUB_WORKSPACE:-.}" diff --no-color --unified=0
  } > "$MUTANT_MARKERS/$(date +%s%N)-$$"
fi

exit "$status"
